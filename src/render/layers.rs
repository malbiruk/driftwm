use smithay::backend::renderer::{
    element::{
        Kind,
        surface::{WaylandSurfaceRenderElement, render_elements_from_surface_tree},
    },
    gles::GlesRenderer,
};
use smithay::desktop::PopupManager;
use smithay::output::Output;
use smithay::reexports::wayland_server::Resource;
use smithay::reexports::wayland_server::backend::ObjectId;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Logical, Physical, Point, Rectangle, Scale};
use smithay::wayland::compositor::with_states;
use smithay::wayland::shell::wlr_layer::Layer as WlrLayer;

use super::blur::{BlurLayer, BlurRequestData};
use super::elements::OutputRenderElements;

/// Push compositor chrome (corner clip on surface, border, shadow) for one
/// layer surface, plus any popup elements anchored to it. Returns the number
/// of render elements emitted *above* the eventual blur insertion point
/// (popups + surface). Border and shadow push after that and are not counted.
///
/// Popups push first (z-order above the surface) and are *not* corner-clipped
/// — they can legitimately extend outside the parent's geometry (tray menus,
/// network dropdowns, tooltips). Mirrors the window pipeline at `mod.rs:312`.
///
/// `push_plain` is the caller's choice of variant for non-clipped surfaces:
/// canvas layers push as `Window` (zoom-rescaled), screen-anchored layers
/// push as `Layer` (no rescale). When `corner_radius > 0`, this function
/// pushes `CsdWindow` regardless — corner clipping wraps in PixelSnap, and at
/// `zoom = 1.0` PixelSnap collapses to identity.
#[allow(clippy::too_many_arguments)]
fn push_layer_chrome(
    target: &mut Vec<OutputRenderElements>,
    state: &mut crate::state::DriftWm,
    applied: Option<&driftwm::config::AppliedWindowRule>,
    surface_id: ObjectId,
    surface_elements: Vec<WaylandSurfaceRenderElement<GlesRenderer>>,
    popup_elements: Vec<WaylandSurfaceRenderElement<GlesRenderer>>,
    inner_logical: Rectangle<f64, Logical>,
    opacity: f64,
    scale: Scale<f64>,
    output_scale: f64,
    zoom: f64,
    mut push_plain: impl FnMut(
        &mut Vec<OutputRenderElements>,
        Vec<WaylandSurfaceRenderElement<GlesRenderer>>,
    ),
) -> usize {
    // Layer-shell surfaces don't have a decoration mode (no titlebar, no
    // SSD/CSD distinction). The `decoration` field is ignored on layer rules;
    // chrome is opt-in field-by-field. `[decorations]` values are NOT
    // inherited for layers — set border_width / corner_radius / shadow
    // explicitly per rule. border_color falls back to the global setting
    // only as a color default when a border width has been set.
    let border_width = applied.and_then(|r| r.border_width).unwrap_or(0);
    let corner_radius = applied.and_then(|r| r.corner_radius).unwrap_or(0);
    let shadow_enabled = applied.and_then(|r| r.shadow).unwrap_or(false);
    let border_color = applied
        .and_then(|r| r.border_color)
        .unwrap_or(state.config.decorations.border_color);

    // Clone shaders so the immutable borrow on `state.render.*_shader` drops
    // before we reborrow `state.render.{border,shadow}_cache` mutably below.
    let corner_clip_shader = state.render.corner_clip_shader.clone();
    let border_shader = state.render.border_shader.clone();
    let shadow_shader = state.render.shadow_shader.clone();

    let chrome_start = target.len();
    // Z-order: earlier-in-vec = nearer the top. Popups go first so they sit
    // above the (possibly corner-clipped) surface body and any border/shadow.
    push_plain(target, popup_elements);

    if corner_radius > 0
        && let Some(ref ccs) = corner_clip_shader
    {
        let r = corner_radius as f32;
        super::push_corner_clipped_elements(
            target,
            surface_elements,
            ccs,
            inner_logical,
            [r, r, r, r],
            zoom,
            output_scale,
        );
    } else {
        push_plain(target, surface_elements);
    }
    let chrome_count = target.len() - chrome_start;

    // Layers don't keyboard-focus as a "current window" — always unfocused color.
    if border_width > 0
        && let Some(ref bs) = border_shader
    {
        super::shaders::push_border_element(
            target,
            &mut state.render.border_cache,
            surface_id.clone(),
            bs,
            inner_logical,
            corner_radius as f32,
            border_width,
            border_color,
            false,
            opacity,
            scale,
            zoom,
        );
    }

    // Shadow inflated by border_width so it grades out from the border's
    // outer perimeter — same pattern as windows in compose_frame.
    if shadow_enabled && let Some(ref ss) = shadow_shader {
        let bw = border_width as f64;
        let body_logical: Rectangle<f64, Logical> = Rectangle::new(
            (inner_logical.loc.x - bw, inner_logical.loc.y - bw).into(),
            (
                inner_logical.size.w + 2.0 * bw,
                inner_logical.size.h + 2.0 * bw,
            )
                .into(),
        );
        super::shaders::push_shadow_element(
            target,
            &mut state.render.shadow_cache,
            surface_id,
            ss,
            body_logical,
            (corner_radius + border_width) as f32,
            opacity,
            scale,
            zoom,
        );
    }

    chrome_count
}

/// Walk popups attached to the layer surface and produce render elements at
/// the same physical anchor as the parent. Mirrors what
/// `LayerSurface::AsRenderElements` does internally, but as a standalone
/// function so the caller can corner-clip the parent surface tree without
/// also clipping these popups.
fn collect_layer_popup_elements(
    renderer: &mut GlesRenderer,
    parent: &WlSurface,
    parent_loc: Point<i32, Physical>,
    scale: Scale<f64>,
    opacity: f32,
) -> Vec<WaylandSurfaceRenderElement<GlesRenderer>> {
    let mut popups = Vec::new();
    for (popup, popup_offset) in PopupManager::popups_for_surface(parent) {
        // Same rounding idiom as the window pipeline's popup walker in
        // mod.rs so layer-shell popups land on the same pixel as xdg popups
        // at fractional output scales.
        let offset: Point<i32, Physical> =
            (popup_offset - popup.geometry().loc).to_physical_precise_round(scale);
        popups.extend(render_elements_from_surface_tree::<
            _,
            WaylandSurfaceRenderElement<GlesRenderer>,
        >(
            renderer,
            popup.wl_surface(),
            parent_loc + offset,
            scale,
            opacity,
            Kind::Unspecified,
        ));
    }
    popups
}

/// Build render elements for canvas-positioned layer surfaces (zoomed like windows).
/// Resolves chrome per-instance via `resolve_window_rules_for_layer_instance` so
/// multi-instance layer-shells (e.g. two waybar bars at different positions) pick
/// up only their own rule's chrome.
pub(super) fn build_canvas_layer_elements(
    state: &mut crate::state::DriftWm,
    renderer: &mut GlesRenderer,
    output_scale: f64,
    camera: Point<f64, Logical>,
    zoom: f64,
    visible_rect: Rectangle<i32, Logical>,
) -> Vec<OutputRenderElements> {
    let scale: Scale<f64> = output_scale.into();
    let mut elements = Vec::new();

    // Topmost first — element lists are front-to-back (see
    // `canvas_layer_indices_sorted` for the z-order contract).
    for idx in state.canvas_layer_indices_sorted() {
        let (surface_id, inner_logical, physical_loc) = {
            let cl = &state.canvas_layers[idx];
            let Some(pos) = cl.position else { continue };
            let bbox = cl.surface.bbox();
            if bbox.size.w <= 0 || bbox.size.h <= 0 {
                continue;
            }
            // Cull off-viewport widgets: an animated widget parked off-screen
            // would otherwise re-render and re-compose its output at full FPS.
            // Popups included, so a menu overhanging the widget body still
            // renders when only the menu is on-screen.
            let mut canvas_bbox = cl.surface.bbox_with_popups();
            canvas_bbox.loc += pos;
            if !visible_rect.overlaps(canvas_bbox) {
                continue;
            }
            let rel_x = pos.x as f64 - camera.x;
            let rel_y = pos.y as f64 - camera.y;
            let physical_loc =
                Point::<f64, Logical>::from((rel_x, rel_y)).to_physical_precise_round(output_scale);
            let inner_logical: Rectangle<f64, Logical> = Rectangle::new(
                (rel_x + bbox.loc.x as f64, rel_y + bbox.loc.y as f64).into(),
                (bbox.size.w as f64, bbox.size.h as f64).into(),
            );
            (cl.surface.wl_surface().id(), inner_logical, physical_loc)
        };

        // Instance index = number of same-namespace canvas layers before this
        // one in the Vec. Matches the `existing_count` computed in
        // `new_layer_surface`, so creation-time and render-time rule lookups
        // resolve to the same positioned rule.
        let instance_idx = {
            let ns = state.canvas_layers[idx].namespace.as_str();
            state.canvas_layers[..idx]
                .iter()
                .filter(|cl| cl.namespace.as_str() == ns)
                .count()
        };
        let applied = state.config.resolve_window_rules_for_layer_instance(
            state.canvas_layers[idx].namespace.as_str(),
            "",
            instance_idx,
        );
        let opacity = applied.as_ref().and_then(|r| r.opacity).unwrap_or(1.0);

        let wl_surface = state.canvas_layers[idx].surface.wl_surface().clone();
        let surface_elements =
            render_elements_from_surface_tree::<_, WaylandSurfaceRenderElement<GlesRenderer>>(
                renderer,
                &wl_surface,
                physical_loc,
                scale,
                opacity as f32,
                Kind::Unspecified,
            );
        let popup_elements = collect_layer_popup_elements(
            renderer,
            &wl_surface,
            physical_loc,
            scale,
            opacity as f32,
        );

        let _ = push_layer_chrome(
            &mut elements,
            state,
            applied.as_ref(),
            surface_id,
            surface_elements,
            popup_elements,
            inner_logical,
            opacity,
            scale,
            output_scale,
            zoom,
            |target, elems| super::push_plain_elements(target, elems, zoom),
        );
    }

    elements
}

/// Build render elements for all layer surfaces on the given layer.
/// Layer surfaces are screen-fixed (not zoomed). Window rules can opt into
/// compositor chrome (corner clip, border, shadow) via `decoration` /
/// `border_width` / `corner_radius` / `shadow` — same set of fields as
/// windows and canvas layers.
///
/// When `blur_layer_tag` is `Some`, layer surfaces whose `namespace()` matches
/// a window rule with `blur = true` (or that have a client-provided blur
/// region) will produce `BlurRequestData` entries alongside their render
/// elements. Blur is captured behind the surface elements only; chrome
/// elements (border / shadow) are pushed *after* the blur request snapshot so
/// the blur insertion point stays at the surface boundary.
pub(super) fn build_layer_elements(
    state: &mut crate::state::DriftWm,
    output: &Output,
    renderer: &mut GlesRenderer,
    layer: WlrLayer,
    blur_layer_tag: Option<BlurLayer>,
) -> (Vec<OutputRenderElements>, Vec<BlurRequestData>) {
    let output_scale = output.current_scale().fractional_scale();
    let scale: Scale<f64> = output_scale.into();
    let mut elements = Vec::new();
    let mut blur_requests = Vec::new();

    let blur_enabled = blur_layer_tag.is_some()
        && state.render.blur_down_shader.is_some()
        && state.render.blur_up_shader.is_some()
        && state.render.blur_mask_shader.is_some();

    // Topmost first — element lists are front-to-back. Shares the z-order
    // (map order + `layer_order` rules) with hit-testing and focus scans.
    let layer_surfaces = state.layers_on_sorted(output, layer);

    for (surface, geo) in layer_surfaces {
        let loc = geo.loc.to_physical_precise_round(output_scale);

        let applied = state.config.resolve_window_rules(surface.namespace(), "");
        let opacity = applied.as_ref().and_then(|r| r.opacity).unwrap_or(1.0);

        // Split surface tree from popups so corner-clip and border only wrap
        // the bar itself — popups (tray menus, dropdowns, tooltips) can sit
        // outside the bar's geometry and must not be cropped to it.
        let wl_surface = surface.wl_surface();
        let surface_elements =
            render_elements_from_surface_tree::<_, WaylandSurfaceRenderElement<GlesRenderer>>(
                renderer,
                wl_surface,
                loc,
                scale,
                opacity as f32,
                Kind::Unspecified,
            );
        let surface_has_buffer = !surface_elements.is_empty();
        let popup_elements =
            collect_layer_popup_elements(renderer, wl_surface, loc, scale, opacity as f32);

        let surface_id = wl_surface.id();
        let inner_logical: Rectangle<f64, Logical> = Rectangle::new(
            (geo.loc.x as f64, geo.loc.y as f64).into(),
            (geo.size.w as f64, geo.size.h as f64).into(),
        );

        let elem_start = elements.len();
        let chrome_count = push_layer_chrome(
            &mut elements,
            state,
            applied.as_ref(),
            surface_id,
            surface_elements,
            popup_elements,
            inner_logical,
            opacity,
            scale,
            output_scale,
            1.0,
            |target, elems| target.extend(elems.into_iter().map(OutputRenderElements::Layer)),
        );

        if blur_enabled
            && let Some(layer_tag) = blur_layer_tag
            && surface_has_buffer
        {
            let rule_blur = applied.as_ref().is_some_and(|r| r.blur);
            let client_blur_rects = with_states(surface.wl_surface(), |s| {
                crate::handlers::background_effect::get_cached_blur_region(s)
            });
            let client_blur = client_blur_rects.as_ref().is_some_and(|r| !r.is_empty());

            if rule_blur || client_blur {
                let screen_rect = geo.to_physical_precise_round(output_scale);

                let region_rects = if client_blur {
                    let rects = client_blur_rects.as_ref().unwrap();
                    let win_bounds: Rectangle<i32, Physical> =
                        Rectangle::from_size(screen_rect.size);
                    let mut out: Vec<Rectangle<i32, Physical>> = Vec::with_capacity(rects.len());
                    for r in rects.iter() {
                        let x1 = (r.loc.x as f64 * output_scale).round() as i32;
                        let y1 = (r.loc.y as f64 * output_scale).round() as i32;
                        let x2 = ((r.loc.x + r.size.w) as f64 * output_scale).round() as i32;
                        let y2 = ((r.loc.y + r.size.h) as f64 * output_scale).round() as i32;
                        let phys: Rectangle<i32, Physical> =
                            Rectangle::from_extremities((x1, y1), (x2, y2));
                        if let Some(clipped) = phys.intersection(win_bounds) {
                            out.push(clipped);
                        }
                    }
                    if out.is_empty() {
                        None
                    } else {
                        Some(std::sync::Arc::new(out))
                    }
                } else {
                    None
                };

                let skip_clipped_out = client_blur && region_rects.is_none() && !rule_blur;

                if !skip_clipped_out {
                    blur_requests.push(BlurRequestData {
                        surface_id: surface.wl_surface().id(),
                        screen_rect,
                        elem_start,
                        elem_count: chrome_count,
                        layer: layer_tag,
                        region_rects,
                    });
                }
            }
        }
    }

    (elements, blur_requests)
}
