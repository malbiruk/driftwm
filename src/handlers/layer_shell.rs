use smithay::{
    delegate_layer_shell,
    desktop::{self, PopupKind, layer_map_for_output},
    input::pointer::CursorImageStatus,
    reexports::wayland_server::{Resource, protocol::wl_output::WlOutput},
    utils::SERIAL_COUNTER,
    wayland::{
        compositor::with_states,
        seat::WaylandFocus,
        shell::{
            wlr_layer::{
                Layer, LayerSurface, WlrLayerShellHandler, WlrLayerShellState,
            },
            xdg::PopupSurface,
        },
    },
};

use std::sync::atomic::{AtomicBool, Ordering};

/// Toggled in the surface data_map when a layer role is destroyed/recreated.
/// Our pre-commit hook (registered early in `new_surface`) checks this to
/// set full anchors before smithay's validation hook runs on orphaned commits.
pub(crate) struct LayerDestroyedMarker(pub AtomicBool);

use crate::state::{CanvasLayer, DriftWm, FocusTarget};

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
            if let Some(marker) = states.data_map.get::<LayerDestroyedMarker>() {
                marker.0.store(false, Ordering::Relaxed);
            }
        });

        // Resolve output: use requested output or fall back to the first available
        let resolved_output = output
            .as_ref()
            .and_then(|wl_out| {
                let client = wl_out.client()?;
                self.space.outputs().find(|o| {
                    o.client_outputs(&client)
                        .any(|co| co == *wl_out)
                })
            })
            .cloned()
            .or_else(|| self.active_output());

        let Some(resolved_output) = resolved_output else {
            tracing::warn!("No output available for layer surface");
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

            // Send wl_surface.enter so client knows output scale/transform
            if let Some(client) = desktop_surface.wl_surface().client() {
                for co in resolved_output.client_outputs(&client) {
                    desktop_surface.wl_surface().enter(&co);
                }
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

        // Remove from canvas layers if it was one
        self.canvas_layers
            .retain(|cl| cl.surface.wl_surface() != surface.wl_surface());

        // Reset pointer_over_layer — the surface may have been under the pointer.
        // Next motion event will re-evaluate, but this prevents stale state in between.
        self.pointer_over_layer = false;

        // Mark this surface so our early pre-commit hook can set full anchors
        // before smithay's layer-shell validation runs. We can't set anchors here
        // directly because smithay resets cached state to defaults AFTER this callback.
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

        // If this surface had exclusive keyboard focus, return focus to the top window
        let wl_surface = surface.wl_surface().clone();
        let keyboard = self.seat.get_keyboard().unwrap();
        let current_focus = keyboard.current_focus();
        if current_focus.as_ref().is_some_and(|f| f.0 == wl_surface) {
            let serial = SERIAL_COUNTER.next_serial();
            let new_focus = self
                .focus_history
                .first()
                .and_then(|w| w.wl_surface().map(|s| FocusTarget(s.into_owned())));
            keyboard.set_focus(self, new_focus, serial);
        }
    }

    fn new_popup(&mut self, _parent: LayerSurface, popup: PopupSurface) {
        let popup = PopupKind::Xdg(popup);
        self.unconstrain_popup(&popup);

        if let Err(err) = self.popups.track_popup(popup) {
            tracing::warn!("error tracking layer popup: {err}");
        }
    }
}

delegate_layer_shell!(DriftWm);
