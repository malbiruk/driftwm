use std::time::Duration;

use smithay::backend::renderer::element::RenderElementStates;
use smithay::desktop::layer_map_for_output;
use smithay::input::pointer::CursorImageStatus;
use smithay::output::Output;

use driftwm::canvas;

/// Frame-callback heartbeat for off-screen toplevels: at most one callback per
/// this interval, vs. full render rate on-screen. Sending zero callbacks
/// off-screen starves the client's buffer cycle, disconnecting native Wayland
/// clients (EGL swap starvation) and stalling Xwayland ones (#141). 995ms (not a
/// round 1s) matches niri so a per-second client still gets one.
const FRAME_CALLBACK_THROTTLE: Duration = Duration::from_millis(995);

/// Sync foreign-toplevel protocol state with the current window list.
/// Call once per frame iteration (not per-output).
pub fn refresh_foreign_toplevels(state: &mut crate::state::DriftWm) {
    let keyboard = state.seat.get_keyboard().unwrap();
    let focused = keyboard.current_focus().map(|f| f.0);
    // Skip virtual placeholders for disconnected monitors — their wl_output
    // global is gone, so advertising them to new toplevels would reference a
    // proxy clients have already destroyed.
    let outputs: Vec<Output> = state
        .space
        .outputs()
        .filter(|o| !state.disconnected_outputs.contains(&o.name()))
        .cloned()
        .collect();
    driftwm::protocols::foreign_toplevel::refresh::<crate::state::DriftWm>(
        &mut state.foreign_toplevel_state,
        &mut state.foreign_toplevel_list_state,
        &state.stage,
        focused.as_ref(),
        &outputs,
    );
}

/// Frame-callback primary-scanout filter, gating callback rate by visibility.
/// Returning `None` for off-screen surfaces makes smithay fall through to its
/// `FRAME_CALLBACK_THROTTLE`-gated `frame_overdue` path (the heartbeat rate)
/// instead of the full render rate.
fn frame_callback_filter<'a>(
    output: &'a Output,
    on_screen: bool,
) -> impl FnMut(
    &smithay::reexports::wayland_server::protocol::wl_surface::WlSurface,
    &smithay::wayland::compositor::SurfaceData,
) -> Option<Output>
+ Copy
+ 'a {
    move |_surface, _states| {
        if on_screen {
            Some(output.clone())
        } else {
            None
        }
    }
}

/// Update each visible surface's primary-scanout-output to `output`. Smithay
/// uses this to decide where to deliver presentation feedback. Must be called
/// after `compositor.render_frame()` so we have render-element states.
pub fn update_primary_scanout_output(
    state: &crate::state::DriftWm,
    output: &Output,
    states: &RenderElementStates,
) {
    use smithay::desktop::utils::update_surface_primary_scanout_output;
    use smithay::wayland::compositor::TraversalAction;
    use smithay::wayland::compositor::with_surface_tree_downward;

    for window in state.stage.windows() {
        window.with_surfaces(|surface, surface_data| {
            update_surface_primary_scanout_output(
                surface,
                output,
                surface_data,
                None,
                states,
                smithay::backend::renderer::element::default_primary_scanout_output_compare,
            );
        });
    }

    let layer_map = layer_map_for_output(output);
    for layer_surface in layer_map.layers() {
        layer_surface.with_surfaces(|surface, surface_data| {
            update_surface_primary_scanout_output(
                surface,
                output,
                surface_data,
                None,
                states,
                smithay::backend::renderer::element::default_primary_scanout_output_compare,
            );
        });
    }
    drop(layer_map);

    for cl in &state.canvas_layers {
        with_surface_tree_downward(
            cl.surface.wl_surface(),
            (),
            |_, _, _| TraversalAction::DoChildren(()),
            |surface, surface_data, _| {
                update_surface_primary_scanout_output(
                    surface,
                    output,
                    surface_data,
                    None,
                    states,
                    smithay::backend::renderer::element::default_primary_scanout_output_compare,
                );
            },
            |_, _, _| true,
        );
    }

    if let Some(lock_surface) = state.lock_surfaces.get(output) {
        with_surface_tree_downward(
            lock_surface.wl_surface(),
            (),
            |_, _, _| TraversalAction::DoChildren(()),
            |surface, surface_data, _| {
                update_surface_primary_scanout_output(
                    surface,
                    output,
                    surface_data,
                    None,
                    states,
                    smithay::backend::renderer::element::default_primary_scanout_output_compare,
                );
            },
            |_, _, _| true,
        );
    }
}

/// Collect presentation-feedback callbacks from all surfaces visible on `output`.
/// Hand the result to `compositor.queue_frame()` and let `frame_submitted()`
/// return it to be consumed by `presented()` on VBlank.
pub fn take_presentation_feedback(
    state: &crate::state::DriftWm,
    output: &Output,
    states: &RenderElementStates,
) -> smithay::desktop::utils::OutputPresentationFeedback {
    use smithay::desktop::utils::{
        OutputPresentationFeedback, surface_presentation_feedback_flags_from_states,
        surface_primary_scanout_output, take_presentation_feedback_surface_tree,
    };

    let mut feedback = OutputPresentationFeedback::new(output);

    for window in state.stage.windows() {
        window.take_presentation_feedback(
            &mut feedback,
            surface_primary_scanout_output,
            |surface, _| surface_presentation_feedback_flags_from_states(surface, None, states),
        );
    }

    let layer_map = layer_map_for_output(output);
    for layer_surface in layer_map.layers() {
        layer_surface.take_presentation_feedback(
            &mut feedback,
            surface_primary_scanout_output,
            |surface, _| surface_presentation_feedback_flags_from_states(surface, None, states),
        );
    }
    drop(layer_map);

    for cl in &state.canvas_layers {
        take_presentation_feedback_surface_tree(
            cl.surface.wl_surface(),
            &mut feedback,
            surface_primary_scanout_output,
            |surface, _| surface_presentation_feedback_flags_from_states(surface, None, states),
        );
    }

    if let Some(lock_surface) = state.lock_surfaces.get(output) {
        take_presentation_feedback_surface_tree(
            lock_surface.wl_surface(),
            &mut feedback,
            surface_primary_scanout_output,
            |surface, _| surface_presentation_feedback_flags_from_states(surface, None, states),
        );
    }

    feedback
}

/// Post-render: frame callbacks, space cleanup.
pub fn post_render(state: &mut crate::state::DriftWm, output: &Output) {
    let time = state.start_time.elapsed();

    // On-screen windows get callbacks at render rate; off-screen ones get the
    // FRAME_CALLBACK_THROTTLE heartbeat (see frame_callback_filter).
    let (camera, zoom) = {
        let os = crate::state::output_state(output);
        (os.camera, os.zoom)
    };
    let viewport_size = crate::state::output_logical_size(output);
    let visible_rect = canvas::visible_canvas_rect(camera.to_i32_round(), viewport_size, zoom);

    for window in state.stage.windows() {
        let Some(loc) = state.stage.position_of(window) else {
            continue;
        };
        let geom_loc = window.geometry().loc;
        let mut bbox = window.bbox_with_popups();
        bbox.loc += loc - geom_loc;
        let on_screen = visible_rect.overlaps(bbox);

        window.send_frame(
            output,
            time,
            Some(FRAME_CALLBACK_THROTTLE),
            frame_callback_filter(output, on_screen),
        );
    }

    // Layer surface frame callbacks
    {
        let layer_map = layer_map_for_output(output);
        for layer_surface in layer_map.layers() {
            layer_surface.send_frame(
                output,
                time,
                Some(Duration::ZERO),
                frame_callback_filter(output, true),
            );
        }
    }

    // Canvas-positioned widgets pan with the viewport, so throttle them
    // off-screen like toplevels (unlike the screen-fixed layer surfaces above).
    for cl in &state.canvas_layers {
        let on_screen = cl.position.is_none_or(|pos| {
            let sb = cl.surface.bbox_with_popups();
            let bbox = smithay::utils::Rectangle::new(
                (pos.x + sb.loc.x, pos.y + sb.loc.y).into(),
                sb.size,
            );
            visible_rect.overlaps(bbox)
        });
        cl.surface.send_frame(
            output,
            time,
            Some(FRAME_CALLBACK_THROTTLE),
            frame_callback_filter(output, on_screen),
        );
    }

    // Cursor surface frame callbacks (animated cursors need these to advance)
    if let CursorImageStatus::Surface(ref surface) = state.cursor.cursor_status {
        smithay::desktop::utils::send_frames_surface_tree(
            surface,
            output,
            time,
            Some(Duration::ZERO),
            frame_callback_filter(output, true),
        );
    }

    // Lock surface frame callback
    if let Some(lock_surface) = state.lock_surfaces.get(output) {
        smithay::desktop::utils::send_frames_surface_tree(
            lock_surface.wl_surface(),
            output,
            time,
            Some(Duration::ZERO),
            frame_callback_filter(output, true),
        );
    }

    // Cleanup
    state.stage.retain_alive();
    state.refresh_window_outputs();
    state.popups.cleanup();
    layer_map_for_output(output).cleanup();
    #[cfg(debug_assertions)]
    state.verify_stage_invariants();

    state.refresh_idle_inhibit();
}

/// Idle-safety net for the off-screen heartbeat (#141): `post_render` only runs
/// when an output renders (damage-driven under udev), so a fully-idle compositor
/// would never service a mapped-but-off-screen surface. Sharing
/// FRAME_CALLBACK_THROTTLE with `post_render` dedups: a surface already serviced
/// within the interval is skipped, so an active render loop produces no doubles.
/// Covers canvas-positioned surfaces (toplevels and `widget` layer surfaces),
/// which pan off-viewport; screen-anchored panels and lock surfaces are excluded
/// — being screen-fixed, they can't be panned away.
pub fn send_frame_callbacks_fallback(state: &mut crate::state::DriftWm) {
    let time = state.start_time.elapsed();
    // Output is irrelevant: the `|_, _| None` filter never reports a primary
    // scanout, so only the throttle's overdue path can fire.
    let Some(output) = state.space.outputs().next().cloned() else {
        return;
    };
    for window in state.stage.windows() {
        window.send_frame(&output, time, Some(FRAME_CALLBACK_THROTTLE), |_, _| None);
    }
    for cl in &state.canvas_layers {
        cl.surface
            .send_frame(&output, time, Some(FRAME_CALLBACK_THROTTLE), |_, _| None);
    }
}
