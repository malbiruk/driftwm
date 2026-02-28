use std::cell::RefCell;

use crate::grabs::{ResizeState, has_left, has_top};
use crate::handlers::layer_shell::LayerDestroyedMarker;
use crate::state::{ClientState, DriftWm, FocusTarget};
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
            SurfaceAttributes,
        },
        dmabuf::get_dmabuf,
        shell::xdg::XdgToplevelSurfaceData,
        shm::{ShmHandler, ShmState},
    },
};

impl CompositorHandler for DriftWm {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor_state
    }

    fn client_compositor_state<'a>(&self, client: &'a Client) -> &'a CompositorClientState {
        &client.get_data::<ClientState>().unwrap().compositor_state
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
        self.redraw_needed = true;
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
                .insert_source(source, move |_, _, data| {
                    if let Some(client_state) = client.get_data::<ClientState>() {
                        let dh = data.display.handle();
                        client_state
                            .compositor_state
                            .blocker_cleared(&mut data.state, &dh);
                    }
                    Ok(())
                })
                .is_ok();
            if ok {
                add_blocker(surface, blocker);
            }
        }

        // Update renderer surface state (buffer dimensions, surface_view, textures).
        // Without this, bbox_from_surface_tree() can't see any surfaces and returns 0x0.
        smithay::backend::renderer::utils::on_commit_buffer_handler::<DriftWm>(surface);

        // For subsurfaces, walk up to root and notify the window
        if !is_sync_subsurface(surface) {
            let mut root = surface.clone();
            while let Some(parent) = get_parent(&root) {
                root = parent;
            }
            let window = self
                .space
                .elements()
                .find(|w| w.toplevel().unwrap().wl_surface() == &root)
                .cloned();
            if let Some(window) = window {
                window.on_commit();

                // Center window on first commit once size is known
                if self.pending_center.remove(&root) {
                    let geo = window.geometry();
                    if geo.size.w > 0 && geo.size.h > 0 {
                        let output_geo = {
                            let output = self.space.outputs().next().cloned();
                            output.and_then(|o| self.space.output_geometry(&o))
                        };
                        if let Some(output_geo) = output_geo {
                            let cx = (self.camera.x + output_geo.size.w as f64 / (2.0 * self.zoom)) as i32 - geo.size.w / 2;
                            let cy = (self.camera.y + output_geo.size.h as f64 / (2.0 * self.zoom)) as i32 - geo.size.h / 2;
                            self.space.map_element(window.clone(), (cx, cy), false);
                        }
                    } else {
                        // Not ready yet, retry next commit
                        self.pending_center.insert(root.clone());
                    }
                }

                // During resize, adjust window position for top/left edge drags
                self.handle_resize_commit(&window, &root);
            }
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
        .find(|w| w.toplevel().unwrap().wl_surface() == surface)
    {
        let toplevel = window.toplevel().unwrap();
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

            // Only grab Exclusive focus when the keyboard isn't already on this surface
            // (prevents stealing focus back on every subsequent commit)
            if interactivity == KeyboardInteractivity::Exclusive {
                let keyboard = self.seat.get_keyboard().unwrap();
                let already_focused = keyboard
                    .current_focus()
                    .as_ref()
                    .is_some_and(|f| f.0 == root);
                if !already_focused {
                    let serial = smithay::utils::SERIAL_COUNTER.next_serial();
                    keyboard.set_focus(self, Some(FocusTarget(root)), serial);
                }
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
