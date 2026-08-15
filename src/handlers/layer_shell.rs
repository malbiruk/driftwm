use smithay::{
    delegate_layer_shell,
    desktop::{self, PopupKind, layer_map_for_output},
    input::pointer::CursorImageStatus,
    reexports::wayland_server::{Resource, protocol::wl_output::WlOutput},
    utils::SERIAL_COUNTER,
    wayland::{
        compositor::with_states,
        shell::{
            wlr_layer::{
                Anchor, Layer, LayerSurface, LayerSurfaceCachedState, WlrLayerShellHandler,
                WlrLayerShellState,
            },
            xdg::PopupSurface,
        },
    },
};

use std::sync::atomic::{AtomicBool, Ordering};

/// Set in the surface data_map when a layer role on a `wl_surface` is
/// destroyed, cleared when it takes a new one. While set, our pre-commit hook
/// (registered early in `new_surface`) strips this surface's buffers and forces
/// full anchors, so smithay's validation hook cannot post an error on the
/// destroyed proxy. The surface is therefore deliberately unmappable until
/// the next `get_layer_surface`.
pub(crate) struct LayerDestroyedMarker(pub AtomicBool);

use crate::state::{CanvasLayer, DriftWm};

impl WlrLayerShellHandler for DriftWm {
    fn shell_state(&mut self) -> &mut WlrLayerShellState {
        &mut self.layer_shell_state
    }

    fn new_layer_surface(
        &mut self,
        surface: LayerSurface,
        output: Option<WlOutput>,
        _layer: Layer,
        namespace: String,
    ) {
        tracing::info!("New layer surface: {namespace}");

        // New surface arrived — clear loading cursor
        if self.cursor.exec_cursor_deadline.take().is_some() {
            self.cursor.exec_cursor_show_at = None;
            self.cursor.cursor_status = CursorImageStatus::default_named();
        }

        // Clear any stale destroyed marker — the wl_surface may be reused
        // (e.g. swayosd destroys and recreates layer surfaces on the same wl_surface)
        with_states(surface.wl_surface(), |states| {
            let was_set = states
                .data_map
                .get::<LayerDestroyedMarker>()
                .is_some_and(|marker| marker.0.swap(false, Ordering::Relaxed));
            // Nothing else undoes the full anchors the hook wrote:
            // LayerSurfaceCachedState's Cacheable::commit returns *self and the
            // cache never resets pending, so they would survive into this role
            // and size it to the whole output. Reset the anchor alone —
            // get_layer_surface has already written .layer into the same state.
            // Both halves: commit is `*self` and merge_into is `*into = self`,
            // so the orphaned commit poisoned current() too, and current() is
            // the half LayerMap::arrange reads via LayerSurface::cached_state().
            if was_set {
                let mut guard = states.cached_state.get::<LayerSurfaceCachedState>();
                guard.pending().anchor = Anchor::empty();
                guard.current().anchor = Anchor::empty();
            }
        });

        // Resolve output: use requested output or fall back to the first available
        let resolved_output = output
            .as_ref()
            .and_then(|wl_out| {
                let client = wl_out.client()?;
                self.space
                    .outputs()
                    .find(|o| o.client_outputs(&client).any(|co| co == *wl_out))
            })
            .cloned()
            .or_else(|| self.active_output());

        let Some(resolved_output) = resolved_output else {
            // Dropping it silently would leave the client holding a live role,
            // waiting on a configure nothing will ever send. Close the contract
            // so it can retry or give up.
            tracing::warn!("No output available for layer surface, closing");
            surface.send_close();
            return;
        };

        // Check if a window rule with position matches this namespace.
        // Count existing canvas layers with the same namespace so the Nth
        // surface matches the Nth rule (supports duplicate app_id rules).
        let existing_count = self
            .canvas_layers
            .iter()
            .filter(|cl| cl.namespace == namespace)
            .count();
        let rule = self
            .config
            .match_window_rule_nth(&namespace, "", existing_count)
            .cloned();
        if let Some(ref rule) = rule
            && let Some((rx, ry)) = rule.position
        {
            let desktop_surface = desktop::LayerSurface::new(surface, namespace.clone());

            // Configure with output width; height left to client
            let output_w = crate::state::output_logical_size(&resolved_output).w;
            desktop_surface.layer_surface().with_pending_state(|state| {
                state.size = Some((output_w, 0).into());
            });
            desktop_surface.layer_surface().send_configure();

            // Output::enter (vs raw wl_surface.enter) tells the client the
            // output's scale/transform and records the surface in enter
            // tracking, so leave_all on output teardown covers canvas layers.
            // Skip a placeholder output: its wl_output global is dead, so
            // entering it would reference a NULL proxy.
            if !self.disconnected_outputs.contains(&resolved_output.name()) {
                resolved_output.enter(desktop_surface.wl_surface());
            }

            self.canvas_layers.push(CanvasLayer {
                surface: desktop_surface,
                rule_position: (rx, ry),
                position: None,
                namespace,
            });
            return;
        }

        // Normal layer surface — map into LayerMap as before
        let desktop_surface = desktop::LayerSurface::new(surface, namespace);

        let mut map = layer_map_for_output(&resolved_output);
        if let Err(e) = map.map_layer(&desktop_surface) {
            tracing::warn!("Failed to map layer surface: {e}");
        }
    }

    fn layer_destroyed(&mut self, surface: LayerSurface) {
        tracing::info!("Layer surface destroyed");

        // Unmap immediately instead of leaving it to post_render's cleanup: a
        // client that takes a fresh role on the same wl_surface before the
        // next frame would otherwise get a second map entry behind the dead
        // one, and since lookups match by wl_surface in map order, its
        // initial configure would be sent on the destroyed proxy.
        let mut unmapped_from = None;
        for output in self.space.outputs() {
            let mut map = layer_map_for_output(output);
            // By role, not by `LayerSurface` equality — that compares the
            // wl_surface alone, which is what put two entries here to begin with.
            let mapped = map
                .layers()
                .find(|l| l.layer_surface().shell_surface() == surface.shell_surface())
                .cloned();
            if let Some(layer) = mapped {
                map.unmap_layer(&layer);
                unmapped_from = Some(output.clone());
                break;
            }
        }

        // Unmapping re-arranges the surviving layers and gives back the
        // exclusive zone straight away, so the output owes a frame for it.
        if let Some(output) = unmapped_from {
            self.redraws_needed.insert(output);
        }

        // Drop any chrome cache entries this layer accumulated. No-op for
        // screen-anchored layers — they never enter these caches — and for
        // canvas layers without chrome opted in via window rule.
        let surface_id = crate::decorations::DecorationKey::Surface(surface.wl_surface().id());
        self.render.border_cache.remove(&surface_id);
        self.render.shadow_cache.remove(&surface_id);

        // Remove from canvas layers if it was one
        self.canvas_layers
            .retain(|cl| cl.surface.wl_surface() != surface.wl_surface());

        // Mark this surface so our early pre-commit hook can neutralise the
        // orphaned commits before smithay's layer-shell validation runs. We
        // can't fix the cached state here directly because smithay resets it to
        // defaults AFTER this callback.
        with_states(surface.wl_surface(), |states| {
            states
                .data_map
                .insert_if_missing_threadsafe(|| LayerDestroyedMarker(AtomicBool::new(false)));
            states
                .data_map
                .get::<LayerDestroyedMarker>()
                .unwrap()
                .0
                .store(true, Ordering::Relaxed);
        });

        // The surface may have been under the pointer. `pointer_over_layer` and
        // smithay's pointer focus are only refreshed by real pointer motion, so
        // without this a layer destroyed under a stationary cursor would route
        // the next press/scroll to the canvas instead of the layer surface still
        // beneath it. The layer was unmapped above, so the recompute lands on
        // whatever is genuinely under the cursor. No-op while locked — `unlock`
        // re-seats pointer focus anyway.
        self.refresh_pointer_focus();

        // Drop on-demand tracking if it pointed at this surface, then recompute
        // focus — it falls back to the next layer or the focused window.
        if self.on_demand_layer.as_ref() == Some(surface.wl_surface()) {
            self.on_demand_layer = None;
        }
        self.update_keyboard_focus(SERIAL_COUNTER.next_serial());
    }

    fn new_popup(&mut self, _parent: LayerSurface, popup: PopupSurface) {
        // No track_popup here: the xdg new_popup handler already queued this
        // popup (parentless at that point) into the manager's unmapped list,
        // and its first commit maps it — with the parent this request just
        // set. Tracking again would double-register it in the popup tree and
        // every layer popup would render twice.
        let popup = PopupKind::Xdg(popup);
        self.unconstrain_popup(&popup);
    }
}

delegate_layer_shell!(DriftWm);
