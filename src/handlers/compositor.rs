use std::cell::RefCell;

use crate::decorations::DecorationKey;
use crate::grabs::{ResizeState, has_left, has_top};
use crate::handlers::layer_shell::LayerDestroyedMarker;
use crate::state::{ClientState, DriftWm, FocusTarget, PendingRecenter, StageWindow};
use driftwm::window_ext::WindowExt;
use smithay::backend::renderer::utils::RendererSurfaceStateUserData;
use smithay::desktop::layer_map_for_output;
use smithay::utils::{Point, Rectangle};
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
        // shutdown, but a client crash destroys wl_surface without it. If the
        // window is still mapped here (no toplevel_destroyed ran), flatten its
        // close animation before cleanup discards the captured textures. Route
        // through the fullscreen lookup so a crash-while-fullscreen fades
        // screen-space on its home output instead of at the parked camera origin
        // (`reap_dead_fullscreen` below still tears the entry down).
        if let Some(window) = self.window_for_surface(surface) {
            let fs_output = self.find_fullscreen_output_for_surface(surface);
            self.snapshot_closing_window(&window, surface, fs_output.as_ref(), false);
        }
        self.cleanup_surface_state(surface);
        // lock_surfaces is keyed by output — sweep values.
        self.lock_surfaces
            .retain(|_, ls| ls.wl_surface() != surface);
        self.stage
            .remove_from_history_matching(|w| w.wl_surface().as_deref() == Some(surface));
        self.reap_dead_fullscreen();
    }

    fn new_surface(
        &mut self,
        surface: &smithay::reexports::wayland_server::protocol::wl_surface::WlSurface,
    ) {
        // Registered before get_layer_surface installs smithay's validation
        // hook, so this fires first. smithay never unregisters that hook, so
        // every commit after a layer role is destroyed still runs it against
        // the state `destroyed` zeroed, posting errors on the dead proxy.
        // Neutralise those commits: full anchors satisfy size validation, and
        // dropping the buffer satisfies the ack-before-attach check — which
        // is what killed clients that re-arm an OSD.
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
                    drop(guard);
                    // Removed rather than None: it also drives
                    // on_commit_buffer_handler through RendererSurfaceState::reset,
                    // which drops — and so releases — the buffer committed
                    // before the destroy. None would leak that one.
                    //
                    // Chained, not bound to a variable: holding this guard
                    // while we lock the renderer state below would take the
                    // two locks in the reverse order on_commit_buffer_handler
                    // uses.
                    let discarded = states
                        .cached_state
                        .get::<SurfaceAttributes>()
                        .pending()
                        .buffer
                        .replace(BufferAssignment::Removed);
                    if let Some(BufferAssignment::NewBuffer(buffer)) = discarded {
                        // Nothing else releases what we discard here:
                        // release-on-replace lives in SurfaceAttributes'
                        // merge_into, which runs on the cache merge, not on a
                        // hook write. A client recycling a one-buffer shm pool
                        // would wait forever.
                        //
                        // Skip the buffer the renderer is still holding —
                        // reset() above already releases it, so releasing it
                        // again here would double-release a buffer the client
                        // re-attached. This still misses one case, a same
                        // buffer queued behind a transaction blocker
                        // releasing it again later, but a doubled release
                        // isn't a protocol error.
                        let held_by_renderer = states
                            .data_map
                            .get::<RendererSurfaceStateUserData>()
                            .is_some_and(|data| {
                                data.lock().unwrap().buffer().is_some_and(|b| b == buffer)
                            });
                        if !held_by_renderer {
                            buffer.release();
                        }
                    }
                }
            });
        });

        // Snapshot a mapped toplevel's markless-conversion inputs the instant it
        // unmaps. Registered before smithay's xdg role-reset hook (which fires on
        // the null-buffer commit and wipes app_id / title / geometry), so a
        // client that unmaps before destroying still converts under
        // `suspend_on_close`.
        add_pre_commit_hook::<DriftWm, _>(surface, |state, _dh, surface| {
            state.capture_unmap_snapshot(surface);
            // Clone the still-imported textures on buffer removal so the close
            // animation can flatten them after teardown (renderer-gated).
            state.capture_close_pixels_on_unmap(surface);
            // The mirror case: on a *new* buffer for a window frozen by a
            // compositor resize, clone the content it is replacing so the leg can
            // crossfade out of it.
            state.stash_resize_content(surface);
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
        let csd_corner_carve = !self
            .decorations
            .contains_key(&DecorationKey::Surface(surface.id()))
            && {
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
                // The locker is consumed by whatever eventually sends `locked` —
                // take it out of the enum. The lock object outlives it in
                // `Locked`, where a later lock request reads its liveness.
                let old =
                    std::mem::replace(&mut self.session_lock, crate::state::SessionLock::Unlocked);
                if let crate::state::SessionLock::Pending(locker) = old {
                    self.enter_locked(locker);
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
            let window = self.window_for_surface(&root);
            if let Some(window) = window {
                window.on_commit();
                // A commit that acks the requested size (or picks a different
                // one) resolves an in-flight geometry chase to the live rect.
                self.resolve_window_animation_commit(&window);

                if self.pending_center.remove(&root) {
                    let geo = window.geometry();
                    let has_size = geo.size.w > 0 && geo.size.h > 0;
                    let is_fullscreen = self.stage.is_fullscreen(&window);

                    // A relaunched app's first sized commit adopts a pending
                    // suspended window: it takes that window's slot instead of
                    // being placed fresh. Resolved (and the token stash
                    // consumed) here so it precedes all placement below.
                    let mut adopted_sid = if has_size {
                        self.adoption_target(&root, &window)
                    } else {
                        None
                    };
                    // Taking a stand-in the user is dragging would destroy it
                    // under the grab driving it. Place the window normally
                    // instead — a coherent state it can sit in indefinitely —
                    // and move it into the slot once the grab lets go.
                    if let Some(sid) = adopted_sid
                        && self.adopt_fights_a_grab(&window, sid)
                    {
                        self.defer_adoption(&root, sid, crate::state::AdoptOrigin::FirstCommit);
                        // Asymmetric on purpose: clearing `adopted_sid` lets the
                        // chain below place the window, while `hidden_for_adopt`
                        // below keeps the arms that establish a *membership*
                        // off — pinning it or sending it fullscreen for the
                        // duration is exactly what the flush's carve-outs would
                        // then dismiss the stand-in for.
                        adopted_sid = None;
                    }
                    // Read off the stash rather than off the branch above: a
                    // rule that forces a size configures and runs this whole
                    // block again on the follow-up commit, and by then the
                    // token stash is spent and the identity fallback may have
                    // lapsed. A pass that re-derived the deferral from the
                    // match would miss it and run the whole non-adopted tail:
                    // the membership arms, and `navigate_to_window`'s camera
                    // flight — the exact flight the deferral exists to avoid,
                    // warping the pointer into the grab that is still live, and
                    // the focus and activation a window nobody can see must not
                    // hold. The rest of that route runs on every pass either
                    // way, since it keys off `adopted_sid` alone: the snap-rect
                    // refresh at the normal placement (which finds no rect to
                    // write while the window is hidden, and which the adopt
                    // clears in any case) and a second open animation, replayed
                    // at the reveal.
                    let hidden_for_adopt = self.root_hidden_by_deferred_adopt(&root);
                    if hidden_for_adopt {
                        // The client's own startup fullscreen/fit goes too, on
                        // every pass: `pending_center` is set again between
                        // passes, so a request arriving there queues rather than
                        // applying. Unlike the immediate adopt's drop
                        // (which trades the request for the slot the window does
                        // end up in), a deferred adopt may never land — a client
                        // that asked before its first buffer then keeps the plain
                        // window it was given. The suppressed
                        // `pinned_to_screen`/`fullscreen` *rules* share that
                        // fate: this is the last placement pass they get, so a
                        // deferral the flush discards (relaunch TTL swept under
                        // the grab) leaves the window plain with nothing left to
                        // re-apply them. What survives is a request the client
                        // makes *after* the last pass, once it is a running app
                        // rather than a starting one: nothing here can reach that
                        // one, and the reveal hands it over.
                        self.pending_fullscreen.remove(&root);
                        self.pending_fit.remove(&root);
                    }

                    // Capture preferred size once; later updated only on
                    // user resize-grab completion. Adoption sets its own
                    // restore size (the body rect).
                    if has_size
                        && !self.stage.is_fit(&window)
                        && !is_fullscreen
                        && adopted_sid.is_none()
                    {
                        self.stage.set_restore_size_if_missing(&window, geo.size);
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

                    // `focus_on_open = false` maps the window without focus or
                    // camera movement; it still takes focus later through normal
                    // interaction (hover with focus-follows-mouse, or a click).
                    let suppress_focus_on_open = applied
                        .as_ref()
                        .is_some_and(|a| a.focus_on_open == Some(false));

                    let mut placed_at_cursor = false;
                    let mut place_in_background = false;
                    // One-shot: when a rule forces a size, first commit
                    // arrives at the client's preferred size; configure with
                    // the rule size and defer positioning/decoration/nav to
                    // the follow-up commit. `pending_size` gate prevents
                    // re-forcing later, so the user can still resize.
                    let mut force_pending = false;

                    if let Some(sid) = adopted_sid {
                        // The body-size configure rides the decoration tail's
                        // `send_configure` below; placement, cursor/auto/background
                        // positioning, and the fullscreen-background check are
                        // all skipped for an adopted window.
                        self.adopt_relaunched(&window, &root, sid);
                        // An adopted window keeps the stand-in's canvas rect;
                        // drop any fullscreen/fit intent the client queued
                        // before its first commit so it can't apply against the
                        // adopted slot later (the rule-driven insert below is
                        // skipped for the adopted case too).
                        self.pending_fullscreen.remove(&root);
                        self.pending_fit.remove(&root);
                    } else if let Some(ref applied) = applied
                        && let Some((w, h)) = applied.size
                        && self.pending_size.insert(root.clone())
                    {
                        if let Some(toplevel) = window.toplevel() {
                            // The rule names a visual frame; configure the content
                            // inside it. The 1px floor matters: xdg-shell reads a
                            // zero dimension as "client picks its own", so a frame
                            // smaller than its own chrome would drop the rule.
                            let content = self
                                .mapping_chrome(&root, &effective)
                                .content_size(smithay::utils::Size::from((w, h)));
                            toplevel.with_pending_state(|state| {
                                state.size = Some(content);
                            });
                            toplevel.send_configure();
                            self.pending_center.insert(root.clone());
                            force_pending = true;
                        } else {
                            self.pending_size.remove(&root);
                        }
                    } else if !hidden_for_adopt
                        && applied.as_ref().is_some_and(|a| a.pinned_to_screen)
                        && has_size
                        && !is_fullscreen
                        && let Some(output) = applied
                            .as_ref()
                            .and_then(|a| a.output.as_deref())
                            .and_then(|name| self.output_by_name(name))
                            .or_else(|| self.active_output())
                    {
                        // Screen-pinned: live in the chosen output's screen
                        // space, not the canvas. A rule `output` picks the
                        // display (else the active one); `position` (if any) is
                        // that output's center, Y-up (output center = origin).
                        let (rx, ry) = applied.as_ref().and_then(|a| a.position).unwrap_or((0, 0));
                        let out_size = crate::state::output_logical_size(&output);
                        // Clamp the top-left into the output so an off-screen rule
                        // `position` (e.g. [1000, 1000] on a 1080p monitor) still
                        // lands fully visible. The rule's rect is the visual
                        // frame, so that is what gets clamped — clamping the
                        // content instead pushes an SSD title bar off the top.
                        let chrome = self.mapping_chrome(&root, &effective);
                        let frame_top_left = driftwm::canvas::rule_to_screen_top_left(
                            rx,
                            ry,
                            chrome.frame_size(geo.size),
                            out_size,
                        );
                        let screen_pos = crate::state::clamp_pin_frame(
                            chrome.content_loc(frame_top_left),
                            geo.size,
                            out_size,
                            chrome,
                        );
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
                        let activate =
                            !suppress_focus_on_open && applied.as_ref().is_none_or(|a| !a.widget);
                        self.map_window(window.clone(), canvas, false);
                        if activate {
                            self.activate_riding_batch(&window);
                        }
                        self.stage.set_pin(
                            &window,
                            driftwm::stage::PinnedSite {
                                output: output.name(),
                                screen_pos,
                            },
                        );
                    } else if has_size && !is_fullscreen && !self.stage.is_fit(&window) {
                        // Fullscreen / fit windows already sit at their final
                        // location — skip positioning so bar-shifted
                        // centering doesn't override that.
                        let chrome = self.mapping_chrome(&root, &effective);
                        let pos = if let Some(ref applied) = applied
                            && let Some((x, y)) = applied.position
                        {
                            let p = driftwm::canvas::rule_to_content(x, y, geo.size, chrome);
                            (p.x, p.y)
                        } else if let Some(parent_surface) = window.parent_surface()
                            && let Some(parent_win) = self.window_for_surface(&parent_surface)
                            && let Some(parent_loc) = self.stage.position_of(&parent_win)
                        {
                            let parent_size = parent_win.geometry().size;
                            (
                                parent_loc.x + parent_size.w / 2 - geo.size.w / 2,
                                parent_loc.y + parent_size.h / 2 - geo.size.h / 2,
                            )
                        } else {
                            // Fullscreen takes precedence over the auto/cursor/
                            // center placement handled here: a new window must
                            // never land on top of a fullscreen window on its own
                            // output.
                            let bg_pos = self.fullscreen_background_pos(&window, geo.size, chrome);
                            place_in_background = bg_pos.is_some();
                            let cursor_pos = if bg_pos.is_none()
                                && matches!(
                                    self.config.window_placement,
                                    driftwm::config::WindowPlacement::Cursor
                                ) {
                                self.cursor_placement_pos(geo.size, chrome)
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
                                self.auto_placement_pos(&window, geo.size, chrome)
                            } else {
                                None
                            };
                            let placed = bg_pos.or(cursor_pos).or(auto_pos).unwrap_or_else(|| {
                                let output_geo = self
                                    .active_output()
                                    .and_then(|o| self.space.output_geometry(&o));
                                if output_geo.is_some() {
                                    // A border is symmetric and cancels out of a
                                    // center; only the bar shifts it.
                                    let bar_f = chrome.bar as f64;
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
                        // fullscreen window focused and on top. Activation rides
                        // the batched configure below instead of a standalone hint.
                        // A deferred adopt is not on screen yet, so it takes the
                        // hint at its reveal instead — same shape as
                        // `focus_on_open = false`.
                        let activate = !place_in_background
                            && !suppress_focus_on_open
                            && !hidden_for_adopt
                            && applied.as_ref().is_none_or(|a| !a.widget);
                        self.map_window(window.clone(), pos.into(), false);
                        if activate {
                            self.activate_riding_batch(&window);
                        }
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
                            self.stage.drop_from_focus_history(&window);
                            if let Some(prev) = self.stage.focus_history().first().cloned() {
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
                            && !self
                                .decorations
                                .contains_key(&DecorationKey::Surface(root.id()))
                        {
                            let deco = crate::decorations::WindowDecoration::new(
                                geo.size.w,
                                true,
                                &self.config.decorations,
                            );
                            self.decorations
                                .insert(DecorationKey::Surface(root.id()), deco);
                        }
                        if adopted_sid.is_none()
                            && !hidden_for_adopt
                            && applied.as_ref().is_some_and(|a| a.fullscreen == Some(true))
                        {
                            self.pending_fullscreen.entry(root.clone()).or_insert(None);
                        }

                        let is_widget = applied.as_ref().is_some_and(|a| a.widget);
                        // Deferred fit/fullscreen will override camera/zoom/raise
                        // /focus — skip navigate_to_window then. Pinned windows
                        // have no canvas position to navigate the camera to.
                        let deferred_fit_or_fs = self.pending_fit.contains(&root)
                            || self.pending_fullscreen.contains_key(&root);
                        // Adopted windows keep the suspended rect and z-slot —
                        // never navigate the camera or raise on adopt. A window
                        // still waiting on a deferred adopt needs no term of its
                        // own: the primitives below refuse to pan, raise or
                        // focus one whatever route reaches them.
                        if !is_widget
                            && !suppress_focus_on_open
                            && !is_fullscreen
                            && !place_in_background
                            && !deferred_fit_or_fs
                            && adopted_sid.is_none()
                        {
                            let reset = self.config.zoom_reset_on_new_window;
                            // Cursor mode is "stay put" by default; only
                            // override in the overview-rescue case (user is
                            // zoomed out and asked for reset).
                            let cursor_overview_rescue =
                                placed_at_cursor && reset && self.zoom() < 1.0 - 1e-9;
                            if self.stage.is_pinned(&window)
                                || placed_at_cursor && !cursor_overview_rescue
                            {
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
                        if adopted_sid.is_none() {
                            // Cache the auto-placed (pre-fit/-fullscreen) rect.
                            // `fit_window_snapped` overwrites with the post-fit
                            // rect; non-snapped fit and fullscreen keep this.
                            // Skipped for an adopted window: its settled rect is
                            // the body size the client hasn't acked yet, so it
                            // establishes a stable rect on its next settle.
                            self.refresh_stable_snap_rect(&StageWindow::Client(window.clone()));
                            // Scale+fade the window in. A window opening straight
                            // into fullscreen/fit runs both in this same commit,
                            // so this entry is never drawn: the geometry entry
                            // below takes its fade over and plays it at the
                            // destination rect instead.
                            self.start_window_open_animation(&window);

                            self.apply_queued_geometry_request(&root);
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
                        let new_loc =
                            crate::state::frame_loc_for_center(target_center, geo.size, bar);
                        self.map_window(window.clone(), new_loc, false);
                        self.refresh_stable_snap_rect(&StageWindow::Client(window.clone()));
                        self.pending_recenter.remove(&root.id());
                    }
                }

                self.settle_adopted_stable_rect(&window, &root);

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
        .stage
        .windows()
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

        // First commit: resolve position once surface size is known. Through the
        // same converter `layer_inventory` reports with, so a rule's position and
        // the position read back describe one rect.
        if self.canvas_layers[idx].position.is_none() {
            let geo = self.canvas_layers[idx].surface.bbox();
            if geo.size.w > 0 && geo.size.h > 0 {
                let (rx, ry) = self.canvas_layers[idx].rule_position;
                let chrome = self.canvas_layer_chrome(idx);
                self.canvas_layers[idx].position =
                    Some(driftwm::canvas::rule_to_content(rx, ry, geo.size, chrome));
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

        let (edges, initial_screen_pos, last_committed_size) = match resize_state {
            ResizeState::Resizing {
                edges,
                initial_screen_pos,
                last_committed_size,
            }
            | ResizeState::WaitingForLastCommit {
                edges,
                initial_screen_pos,
                last_committed_size,
            } => (edges, initial_screen_pos, last_committed_size),
            ResizeState::Idle => return,
        };

        let current_geo = window.geometry();

        // Compensate incrementally from wherever the window is now, against the
        // size this commit replaces. The only job here is to hold the opposite
        // edge still across one size change; deriving the position absolutely
        // from the grab start would also undo anything that placed the window
        // between the release and this commit (a fill, a fit, an exit, an IPC
        // move), and the settle can be owed indefinitely.
        //
        // A placement that changed the size as well as the position owns both,
        // and the delta is then measured against a size the resize never asked
        // for — so skip the compensation and let the placement stand. Each
        // witness is exact: `begin_client_resize` clears fit and fill at entry,
        // so either membership here landed after the grab started, and an owed
        // recenter is how the three exits record having configured a different
        // size. The map itself stays unconditional — it doubles as the resize's
        // z-raise.
        let placement_owns_size = self.stage.is_fill(window)
            || self.stage.is_fit(window)
            || self.is_window_fullscreen(window)
            || self.pending_recenter.contains_key(&surface.id());

        if initial_screen_pos.is_some() {
            // Pinned: top/left-edge resize moves `screen_pos` so the opposite
            // edge stays fixed. The Space loc is re-synced here directly because
            // the per-frame loc-sync only fires on camera changes.
            if let Some(site) = self.stage.pin_of(window).cloned() {
                let mut new_sp = site.screen_pos;
                if !placement_owns_size {
                    if has_top(edges) {
                        new_sp.y = site.screen_pos.y + (last_committed_size.h - current_geo.size.h);
                    }
                    if has_left(edges) {
                        new_sp.x = site.screen_pos.x + (last_committed_size.w - current_geo.size.w);
                    }
                }
                self.stage.set_pin(
                    window,
                    driftwm::stage::PinnedSite {
                        output: site.output.clone(),
                        screen_pos: new_sp,
                    },
                );
                // Output gone: keep the screen_pos update, skip only the
                // loc re-anchor — the tail below must still run to reset
                // ResizeState.
                if let Some(output) = self.output_by_name(&site.output) {
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
                    self.map_window(window.clone(), canvas, false);
                }
            }
        } else if let Some(current_pos) = self.stage.position_of(window) {
            // `if let`, not an early return on `None`: the tail below has to run
            // whatever happens here, or `WaitingForLastCommit` is stranded and
            // every later commit re-runs the settle. The map stays unconditional
            // inside it — it doubles as the resize's z-raise, and skipping it
            // when the delta is zero would silently drop that raise.
            let mut new_loc = current_pos;
            if !placement_owns_size {
                if has_top(edges) {
                    new_loc.y = current_pos.y + (last_committed_size.h - current_geo.size.h);
                }
                if has_left(edges) {
                    new_loc.x = current_pos.x + (last_committed_size.w - current_geo.size.w);
                }
            }
            self.map_window(window.clone(), new_loc, false);
        }

        // Bump the blur generation only when this commit actually changed the
        // committed size. handle_resize_commit runs on every commit of the
        // toplevel, so an unconditional bump would force every frosted window on
        // all outputs to re-blur at a busy client's repaint rate under a
        // held-still resize border. The top/left reposition above is derived
        // from the size delta, so an unchanged committed size means no
        // reposition either.
        let size_changed = current_geo.size != last_committed_size;
        if size_changed {
            self.render.blur_geometry_generation += 1;
        }

        if matches!(resize_state, ResizeState::WaitingForLastCommit { .. }) {
            // Anchor restore_size to the user's final choice so a subsequent
            // fit/fullscreen round-trip restores to this.
            self.stage.set_restore_size(window, current_geo.size);
            with_states(surface, |states| {
                states
                    .data_map
                    .get_or_insert(|| RefCell::new(ResizeState::Idle))
                    .replace(ResizeState::Idle);
            });
            self.refresh_stable_snap_rect(&StageWindow::Client(window.clone()));
            // The grab's `unset` is too early for this: a client resize is
            // witnessed by the surface's own `ResizeState`, which only reaches
            // `Idle` on the commit above, so scheduling at release would flush
            // into a still-live grab and simply defer again. A resize that nets
            // no size change has nothing to configure, so this hook waits on
            // whatever the client commits next — up to the relaunch TTL, whose
            // end state is the documented stale duplicate.
            self.schedule_deferred_adoptions();
        } else if size_changed {
            // Still resizing: carry the new committed size forward so the next
            // commit compares against it (write-back only on change).
            with_states(surface, |states| {
                states
                    .data_map
                    .get_or_insert(|| RefCell::new(ResizeState::Idle))
                    .replace(ResizeState::Resizing {
                        edges,
                        initial_screen_pos,
                        last_committed_size: current_geo.size,
                    });
            });
        }
    }

    /// Pay off the stable snap rect an adopt deferred, on the first commit whose
    /// geometry is the size the adopt configured. Until then the window has no
    /// stable rect at all, which is what keeps `reflow_grown_snapped_window`
    /// (whose whole premise is a footprint that grew past a settled one) off a
    /// client still drawing its pre-adopt size.
    fn settle_adopted_stable_rect(
        &mut self,
        window: &smithay::desktop::Window,
        surface: &smithay::reexports::wayland_server::protocol::wl_surface::WlSurface,
    ) {
        let Some(&adopt_size) = self.pending_adopt_settle.get(&surface.id()) else {
            return;
        };
        // A commit within a pixel of `adopt_size` is a size the reflow's grow
        // test could never act on, so count it as settled rather than hold the
        // debt over a rounding difference.
        const EPS: i32 = 1;
        let size = window.geometry().size;
        if (size.w - adopt_size.w).abs() > EPS || (size.h - adopt_size.h).abs() > EPS {
            return;
        }
        let client = StageWindow::Client(window.clone());
        // A window with no canvas footprint (pinned / fullscreen) has no rect to
        // record, and clearing the debt against a write that never lands would
        // leave it with no stable rect at all. Keep owing until it has one.
        if self.snap_rect_for(&client).is_none() {
            return;
        }
        self.pending_adopt_settle.remove(&surface.id());
        self.refresh_stable_snap_rect(&client);
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
        // A filled window is deliberately grown in place and may retain an
        // unresolvable overlap; reflowing it here would translate it (violating
        // fill's never-move contract) off a now-stale stable snap rect.
        if self.is_window_fullscreen(window)
            || self.stage.is_fit(window)
            || self.stage.is_fill(window)
        {
            return;
        }

        let Some(&stable) = self.stable_snap_rects.get(&surface.id()) else {
            return;
        };
        // `snap_rect_for` returns `None` for widgets / pinned / fullscreen, so
        // this also filters those out.
        let Some(current) = self.snap_rect_for(&StageWindow::Client(window.clone())) else {
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

        // Clients may ack a configure before their resized frame lands, so ack
        // state can't gate this: a surface mid-settle after a fullscreen/fit/fill
        // exit keeps committing stale-sized frames until it resizes.
        if self.pending_recenter.contains_key(&surface.id()) {
            return;
        }

        // A commit that lands while the client still owes the server a resize it
        // configured carries a stale footprint — a window exiting fullscreen
        // keeps committing viewport-sized frames until it acks the restore
        // configure. Reflowing off that stale size would relocate the window, so
        // wait for the settle.
        if crate::state::owes_a_configured_size(window) {
            return;
        }

        let gap = self.config.snap_gap;
        // Every snap-rect citizen — live windows and suspended stand-ins alike —
        // counts as a neighbor; `snap_rect_for` drops widgets / pinned /
        // fullscreen.
        let others: Vec<(StageWindow, driftwm::layout::snap::SnapRect)> = self
            .stage
            .windows()
            .filter_map(|w| {
                if w == window {
                    return None;
                }
                Some((w.clone(), self.snap_rect_for(w)?))
            })
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
        let chrome = self.element_chrome(window);
        let Some((x, y)) = self.place_adjacent_to(&anchor, window, content_size, chrome) else {
            return;
        };
        let new_loc = Point::from((x, y));
        if self.stage.position_of(window) == Some(new_loc) {
            return;
        }
        self.map_window(window.clone(), new_loc, false);
        self.refresh_stable_snap_rect(&StageWindow::Client(window.clone()));

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
