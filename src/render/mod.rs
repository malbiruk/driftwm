mod background;
mod blur;
mod capture;
mod capture_background;
mod cursor;
mod elements;
mod error_bar;
mod layers;
mod lifecycle;
mod screenshot;
mod shader_chunks;
mod shaders;
mod tile_chunks;
mod tile_chunks_tiff;
mod tile_worker;

pub use background::{BackgroundElement, init_background, update_background_element};
pub use blur::BlurCache;
pub(crate) use blur::compile_blur_shaders;
pub use capture::{render_capture_frames, render_screencopy, render_toplevel_captures};
pub use cursor::build_cursor_elements;
pub use elements::{
    OutputRenderElements, PixelSnapRescaleElement, RoundedCornerElement, TileShaderElement,
};
pub use error_bar::ErrorBarCache;
pub use lifecycle::{
    post_render, refresh_foreign_toplevels, send_frame_callbacks_fallback,
    take_presentation_feedback, update_primary_scanout_output,
};
pub use screenshot::capture_region_to_png;
pub use shader_chunks::ShaderChunkCache;
pub use shaders::{
    BorderPhysKey, ShadowPhysKey, compile_border_shader, compile_corner_clip_shader,
    compile_shadow_shader,
};
pub use tile_chunks::BgChunkCache;

use blur::{BlurLayer, BlurRequestData, process_blur_requests};
use layers::{build_canvas_layer_elements, build_layer_elements};
use shaders::{push_border_element, push_shadow_element};

use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::{
    element::{
        AsRenderElements, Kind,
        memory::{MemoryRenderBuffer, MemoryRenderBufferRenderElement},
        surface::WaylandSurfaceRenderElement,
    },
    gles::{GlesRenderer, GlesTexProgram},
};
use smithay::output::Output;
use smithay::reexports::wayland_server::Resource;
use smithay::utils::{IsAlive, Logical, Physical, Point, Rectangle, Scale, Size, Transform};
use smithay::wayland::compositor::with_states;
use smithay::wayland::seat::WaylandFocus;
use smithay::wayland::shell::wlr_layer::Layer as WlrLayer;

use driftwm::canvas;
use driftwm::window_ext::WindowExt;

/// Render elements for a locked session: only the lock surface (the lock
/// client provides its own cursor).
fn compose_lock_frame(
    state: &crate::state::DriftWm,
    renderer: &mut GlesRenderer,
    output: &Output,
    _cursor_elements: Vec<OutputRenderElements>,
) -> Vec<OutputRenderElements> {
    let mut elements = Vec::new();

    if let Some(lock_surface) = state.lock_surfaces.get(output) {
        let output_scale = output.current_scale().fractional_scale();
        let lock_elements =
            smithay::backend::renderer::element::surface::render_elements_from_surface_tree(
                renderer,
                lock_surface.wl_surface(),
                (0, 0),
                Scale::from(output_scale),
                1.0,
                Kind::Unspecified,
            );
        elements.extend(lock_elements.into_iter().map(OutputRenderElements::Layer));
    }

    elements
}

/// Wrap every surface element of a window in the corner-clip shader and push
/// into `target`. The clip applies uniformly to the root toplevel and every
/// subsurface, so clients that render content via subsurfaces (Firefox
/// dmabuf, HW-accelerated video) get rounded corners the same as simple
/// single-surface clients.
///
/// `geometry` is the window's geometry rect in screen-logical pre-zoom
/// coords — i.e. where the content rect ends up on the output before zoom.
/// Pixels outside this rect are discarded by the shader, which doubles as
/// the CSD-shadow strip mask the old `u_clip_shadow` uniform used to do.
///
/// `corner_radius` is per-corner in pre-zoom physical pixels, ordered
/// `(top_left, top_right, bottom_right, bottom_left)`. Pass `0` on any
/// corner that should stay square (e.g. top corners under an SSD title
/// bar).
#[allow(clippy::too_many_arguments)]
fn push_corner_clipped_elements(
    target: &mut Vec<OutputRenderElements>,
    elems: Vec<WaylandSurfaceRenderElement<GlesRenderer>>,
    shader: &GlesTexProgram,
    geometry: Rectangle<f64, Logical>,
    corner_radius: [f32; 4],
    zoom: f64,
    output_scale: f64,
) {
    let aa_scale = (output_scale * zoom) as f32;
    // Clamp radii so a tiny window doesn't get corners wider than half its
    // side. `max_r` is guarded against ≤0 since a degenerate window can
    // briefly have zero size and `clamp(lo, hi)` panics if `lo > hi`.
    let max_r = ((geometry.size.w.min(geometry.size.h) as f32) * 0.5).max(0.0);
    let clamped = [
        corner_radius[0].clamp(0.0, max_r),
        corner_radius[1].clamp(0.0, max_r),
        corner_radius[2].clamp(0.0, max_r),
        corner_radius[3].clamp(0.0, max_r),
    ];
    for elem in elems {
        target.push(OutputRenderElements::CsdWindow(
            PixelSnapRescaleElement::from_element(
                RoundedCornerElement::new(
                    elem,
                    shader.clone(),
                    geometry,
                    clamped,
                    output_scale,
                    aa_scale,
                ),
                Point::<i32, Physical>::from((0, 0)),
                zoom,
            ),
        ));
    }
}

fn push_plain_elements(
    target: &mut Vec<OutputRenderElements>,
    elems: Vec<WaylandSurfaceRenderElement<GlesRenderer>>,
    zoom: f64,
) {
    target.extend(elems.into_iter().map(|elem| {
        OutputRenderElements::Window(PixelSnapRescaleElement::from_element(
            elem,
            Point::<i32, Physical>::from((0, 0)),
            zoom,
        ))
    }));
}

/// Compose windows + SSD chrome + background for a *virtual* viewport at
/// top-left canvas coord `camera`, `dpi_scale` pixels per canvas unit. DPI is
/// folded into the render scale with zoom fixed at 1.0, so elements need no
/// rescale.
///
/// Mirrors `compose_frame`'s per-window chrome but omits blur (the one
/// framebuffer-sampling effect, which would seam across capture tiles) and the
/// live per-output background caches (uses `capture_bg` instead). Everything
/// emitted is a deterministic function of canvas position, so adjacent tiles
/// stitch with no overlap margin. Layer surfaces, cursor, output outlines, and
/// the error bar are excluded.
///
/// Border/shadow elements share the live `border_cache`/`shadow_cache`; a
/// capture's keys (scale=dpi, zoom=1.0) differ from the live frame's, so the
/// next live frame rebuilds those entries once — preferred over a second cache
/// since captures are rare.
pub(crate) fn compose_capture_elements(
    state: &mut crate::state::DriftWm,
    renderer: &mut GlesRenderer,
    camera: Point<f64, Logical>,
    dpi_scale: f64,
    viewport_logical: Size<i32, Logical>,
    capture_bg: &capture_background::CaptureBackground,
) -> Vec<OutputRenderElements> {
    use smithay::backend::renderer::element::surface::render_elements_from_surface_tree;

    let zoom = 1.0;
    let output_scale = dpi_scale;
    let scale = Scale::from(dpi_scale);
    let visible_rect = canvas::visible_canvas_rect(camera.to_i32_round(), viewport_logical, zoom);

    let focused_surface = state
        .seat
        .get_keyboard()
        .and_then(|kb| kb.current_focus())
        .map(|f| f.0);

    let mut normal: Vec<OutputRenderElements> = Vec::new();
    let mut widgets: Vec<OutputRenderElements> = Vec::new();

    // Collect first: the surface-tree calls borrow `state`, which would conflict
    // with an in-flight `state.space.elements()` iterator. Windows are Arc-backed.
    let windows: Vec<smithay::desktop::Window> = state.space.elements().rev().cloned().collect();
    for window in &windows {
        let Some(loc) = state.space.element_location(window) else {
            continue;
        };
        let geom_loc = window.geometry().loc;
        let geom_size = window.geometry().size;
        let Some(wl_surface) = window.wl_surface() else {
            continue;
        };
        let is_fullscreen = state.fullscreen.values().any(|fs| &fs.window == window);
        let has_ssd = !is_fullscreen && state.decorations.contains_key(&wl_surface.id());

        let applied = driftwm::config::applied_rule(&wl_surface);
        let is_widget = applied.as_ref().is_some_and(|r| r.widget);
        let is_focused = focused_surface.as_ref().is_some_and(|f| *f == *wl_surface);
        let opacity = applied.as_ref().and_then(|r| r.opacity).unwrap_or(1.0);

        let effective_mode = driftwm::config::effective_decoration_mode(
            applied.as_ref().and_then(|r| r.decoration.as_ref()),
            &state.config.decorations.default_mode,
        );
        let effective_bw = if is_fullscreen {
            0
        } else {
            driftwm::config::effective_border_width(
                applied.as_ref(),
                effective_mode,
                &state.config.decorations,
            )
        };
        let border_color = if is_focused {
            driftwm::config::effective_border_color_focused(
                applied.as_ref(),
                &state.config.decorations,
            )
        } else {
            driftwm::config::effective_border_color(applied.as_ref(), &state.config.decorations)
        };
        let effective_corner_radius = driftwm::config::effective_corner_radius(
            applied.as_ref(),
            effective_mode,
            &state.config.decorations,
        );
        let effective_shadow = !is_fullscreen
            && driftwm::config::effective_shadow_enabled(
                applied.as_ref(),
                effective_mode,
                &state.config.decorations,
            );

        let mut bbox = window.bbox();
        bbox.loc += loc - geom_loc;
        if has_ssd {
            let r = driftwm::config::DecorationConfig::SHADOW_RADIUS.ceil() as i32;
            let bar = state.config.decorations.title_bar_height;
            bbox.loc.x -= r;
            bbox.loc.y -= bar + r;
            bbox.size.w += 2 * r;
            bbox.size.h += bar + 2 * r;
        }
        if effective_bw > 0 {
            bbox.loc.x -= effective_bw;
            bbox.loc.y -= effective_bw;
            bbox.size.w += 2 * effective_bw;
            bbox.size.h += 2 * effective_bw;
        }
        if !visible_rect.overlaps(bbox) {
            continue;
        }

        // `output: None` => off-screen canvas capture. Pinned windows return
        // None here by construction, so a canvas screenshot never includes a
        // screen-pinned window.
        let Some((render_loc, _)) = state.window_render_transform(window, None, camera, zoom)
        else {
            continue;
        };
        let loc_phys: Point<i32, Physical> = render_loc.to_physical_precise_round(scale);

        let (elems, popup_elems) = if let Some(toplevel) = window.toplevel() {
            let root = toplevel.wl_surface();
            let top =
                render_elements_from_surface_tree::<_, WaylandSurfaceRenderElement<GlesRenderer>>(
                    renderer,
                    root,
                    loc_phys,
                    scale,
                    opacity as f32,
                    Kind::Unspecified,
                );
            let mut popups: Vec<WaylandSurfaceRenderElement<GlesRenderer>> = Vec::new();
            for (popup, popup_offset) in smithay::desktop::PopupManager::popups_for_surface(root) {
                let offset: Point<i32, Physical> = (window.geometry().loc + popup_offset
                    - popup.geometry().loc)
                    .to_physical_precise_round(scale);
                popups.extend(render_elements_from_surface_tree::<
                    _,
                    WaylandSurfaceRenderElement<GlesRenderer>,
                >(
                    renderer,
                    popup.wl_surface(),
                    loc_phys + offset,
                    scale,
                    opacity as f32,
                    Kind::Unspecified,
                ));
            }
            (top, popups)
        } else {
            let elems = window.render_elements::<WaylandSurfaceRenderElement<GlesRenderer>>(
                renderer,
                loc_phys,
                scale,
                opacity as f32,
            );
            (elems, Vec::new())
        };

        let target = if is_widget { &mut widgets } else { &mut normal };
        // Popups push first so they sit above the title bar and window content.
        push_plain_elements(target, popup_elems, zoom);

        if has_ssd {
            let bar_height = state.config.decorations.title_bar_height;
            // Snap the title-bar band to whole physical pixels and share it across the
            // bar, border, and shadow. Deriving each from the raw logical bar_height
            // rounds their common top edge inconsistently at fractional scale
            // (round(t*s) - round(h*s) != round((t-h)*s)) — a ±1px seam.
            let bar_h_phys = (bar_height as f64 * scale.y).round();
            let bar_h_logical = bar_h_phys / scale.y;

            // Reuse the buffer the live frame rasterized (no re-`update`): keeps
            // borrows simple, text is microseconds-stale at worst.
            if let Some(deco) = state.decorations.get(&wl_surface.id()) {
                let bar_physical: Point<f64, Physical> =
                    Point::from((loc_phys.x as f64, loc_phys.y as f64 - bar_h_phys));
                let bar_alpha = if opacity < 1.0 {
                    Some(opacity as f32)
                } else {
                    None
                };
                if let Ok(bar_elem) = MemoryRenderBufferRenderElement::from_buffer(
                    renderer,
                    bar_physical,
                    &deco.title_bar,
                    bar_alpha,
                    None,
                    None,
                    Kind::Unspecified,
                ) {
                    target.push(OutputRenderElements::Decoration(
                        PixelSnapRescaleElement::from_element(
                            bar_elem,
                            Point::<i32, Physical>::from((0, 0)),
                            zoom,
                        ),
                    ));
                }
            }

            // Only bottom corners round (title bar covers the top edge).
            if let Some(ref shader) = state.render.corner_clip_shader {
                let radius = effective_corner_radius as f32;
                if radius > 0.0 {
                    let wg = window.geometry();
                    let geometry = Rectangle::new(
                        Point::<f64, Logical>::from((
                            render_loc.x + wg.loc.x as f64,
                            render_loc.y + wg.loc.y as f64,
                        )),
                        Size::<f64, Logical>::from((wg.size.w as f64, wg.size.h as f64)),
                    );
                    push_corner_clipped_elements(
                        target,
                        elems,
                        shader,
                        geometry,
                        [0.0, 0.0, radius, radius],
                        zoom,
                        output_scale,
                    );
                } else {
                    push_plain_elements(target, elems, zoom);
                }
            } else {
                push_plain_elements(target, elems, zoom);
            }

            if effective_bw > 0
                && let Some(shader) = state.render.border_shader.clone()
            {
                let inner_logical: Rectangle<f64, Logical> = Rectangle::new(
                    (render_loc.x, render_loc.y - bar_h_logical).into(),
                    (geom_size.w as f64, geom_size.h as f64 + bar_h_logical).into(),
                );
                push_border_element(
                    target,
                    &mut state.render.border_cache,
                    wl_surface.id(),
                    &shader,
                    inner_logical,
                    effective_corner_radius as f32,
                    effective_bw,
                    border_color,
                    is_focused,
                    opacity,
                    scale,
                    zoom,
                );
            }

            if effective_shadow && let Some(shader) = state.render.shadow_shader.clone() {
                let bw = effective_bw as f64;
                let body_logical: Rectangle<f64, Logical> = Rectangle::new(
                    (render_loc.x - bw, render_loc.y - bar_h_logical - bw).into(),
                    (
                        geom_size.w as f64 + 2.0 * bw,
                        geom_size.h as f64 + bar_h_logical + 2.0 * bw,
                    )
                        .into(),
                );
                push_shadow_element(
                    target,
                    &mut state.render.shadow_cache,
                    wl_surface.id(),
                    &shader,
                    body_logical,
                    (effective_corner_radius + effective_bw) as f32,
                    opacity,
                    scale,
                    zoom,
                );
            }
        } else if let Some(ref shader) = state.render.corner_clip_shader {
            let geo = window.geometry();
            let radius = effective_corner_radius as f32;
            let bare = matches!(effective_mode, driftwm::config::DecorationMode::None);

            if !bare && !is_fullscreen {
                let geometry = Rectangle::new(
                    Point::<f64, Logical>::from((
                        render_loc.x + geo.loc.x as f64,
                        render_loc.y + geo.loc.y as f64,
                    )),
                    Size::<f64, Logical>::from((geo.size.w as f64, geo.size.h as f64)),
                );
                push_corner_clipped_elements(
                    target,
                    elems,
                    shader,
                    geometry,
                    [radius, radius, radius, radius],
                    zoom,
                    output_scale,
                );

                if effective_bw > 0
                    && let Some(border_shader) = state.render.border_shader.clone()
                {
                    push_border_element(
                        target,
                        &mut state.render.border_cache,
                        wl_surface.id(),
                        &border_shader,
                        geometry,
                        radius,
                        effective_bw,
                        border_color,
                        is_focused,
                        opacity,
                        scale,
                        zoom,
                    );
                }

                if effective_shadow && let Some(shader) = state.render.shadow_shader.clone() {
                    let bw = effective_bw as f64;
                    let body_logical: Rectangle<f64, Logical> = Rectangle::new(
                        (
                            render_loc.x + geo.loc.x as f64 - bw,
                            render_loc.y + geo.loc.y as f64 - bw,
                        )
                            .into(),
                        (geom_size.w as f64 + 2.0 * bw, geom_size.h as f64 + 2.0 * bw).into(),
                    );
                    push_shadow_element(
                        target,
                        &mut state.render.shadow_cache,
                        wl_surface.id(),
                        &shader,
                        body_logical,
                        (effective_corner_radius + effective_bw) as f32,
                        opacity,
                        scale,
                        zoom,
                    );
                }
            } else {
                push_plain_elements(target, elems, zoom);
            }
        } else {
            push_plain_elements(target, elems, zoom);
        }
    }

    // Canvas-positioned layer widgets sit between normal windows and widget
    // toplevels, as in compose_frame. Screen-anchored layer surfaces (panels) are
    // excluded — they aren't canvas content.
    let canvas_layers =
        build_canvas_layer_elements(state, renderer, output_scale, camera, zoom, visible_rect);
    let bg = capture_bg.tile_elements(
        camera,
        viewport_logical,
        state.start_time.elapsed().as_secs_f32(),
    );
    let mut all = Vec::with_capacity(normal.len() + canvas_layers.len() + widgets.len() + bg.len());
    all.extend(normal);
    all.extend(canvas_layers);
    all.extend(widgets);
    all.extend(bg);
    all
}

/// Assemble all render elements for a frame. Caller provides cursor elements
/// (built before taking the renderer).
pub fn compose_frame(
    state: &mut crate::state::DriftWm,
    renderer: &mut GlesRenderer,
    output: &Output,
    cursor_elements: Vec<OutputRenderElements>,
) -> Vec<OutputRenderElements> {
    #[cfg(feature = "profile-with-tracy")]
    let _span = tracy_client::span!("compose_frame");

    if state.dnd_icon.as_ref().is_some_and(|i| !i.surface.alive()) {
        state.dnd_icon = None;
    }

    if !matches!(state.session_lock, crate::state::SessionLock::Unlocked) {
        return compose_lock_frame(state, renderer, output, cursor_elements);
    }

    let name = output.name();
    let output_fullscreen = state.is_output_fullscreen(output);
    // The fullscreen window fully occludes its output: only it, the overlay
    // layer, and the cursor render; everything beneath is culled below. Pinned
    // windows count as top-tier toplevels and get covered like the top layer.
    let fullscreen_window = state.fullscreen.get(output).map(|fs| fs.window.clone());
    let mut did_init_bg = false;
    if output_fullscreen {
        // Fullscreen fully occludes the canvas: free its chunk caches and skip
        // the background. Maximize is NOT fullscreen, so it keeps its background.
        state.render.remove_background_chunks(&name);
    } else if !state.render.cached_bg.contains_key(&name)
        && !state.render.cached_tile_chunks.contains_key(&name)
        && !state.render.cached_shader_chunks.contains_key(&name)
    {
        let output_size = crate::state::output_logical_size(output);
        init_background(state, renderer, output_size, &name);
        did_init_bg = true;
    }

    // Read per-output state directly — active_output() follows the pointer,
    // which is wrong when rendering an output the pointer isn't on.
    let (camera, zoom) = {
        let os = crate::state::output_state(output);
        (os.camera, os.zoom)
    };

    // A just-re-created `cached_bg` carries placeholder camera=(0,0)/zoom=1.0
    // (see `init_background`), so without this it renders one frame at the wrong
    // offset. NaN "last" values force the uniform push (same sentinel as
    // `OutputState`'s initial values). No-op for the chunk caches, which derive
    // geometry from the live camera each frame.
    if did_init_bg {
        update_background_element(
            state,
            output,
            camera,
            zoom,
            Point::from((f64::NAN, f64::NAN)),
            f64::NAN,
        );
    }

    let viewport_size = crate::state::output_logical_size(output);
    let visible_rect = canvas::visible_canvas_rect(camera.to_i32_round(), viewport_size, zoom);
    let output_scale = output.current_scale().fractional_scale();
    let scale = Scale::from(output_scale);

    // Split windows into normal and widget layers so canvas layers render
    // between them. Replicates render_elements_for_region internals.
    let mut zoomed_normal: Vec<OutputRenderElements> = Vec::new();
    let mut zoomed_widgets: Vec<OutputRenderElements> = Vec::new();
    // Screen-pinned windows: own bucket, rendered above normal and below
    // Top/Overlay layer-shell (see all_elements assembly below).
    let mut zoomed_pinned: Vec<OutputRenderElements> = Vec::new();

    let blur_enabled = state.render.blur_down_shader.is_some()
        && state.render.blur_up_shader.is_some()
        && state.render.blur_mask_shader.is_some();
    let mut blur_requests: Vec<BlurRequestData> = Vec::new();

    let focused_surface = state
        .seat
        .get_keyboard()
        .and_then(|kb| kb.current_focus())
        .map(|f| f.0);

    #[cfg(feature = "profile-with-tracy")]
    let _windows_span = tracy_client::span!("compose::windows");
    #[cfg(feature = "profile-with-tracy")]
    let (mut visible_windows, mut shadow_elems) = (0u32, 0u32);
    for window in state.space.elements().rev() {
        let Some(loc) = state.space.element_location(window) else {
            continue;
        };
        if output_fullscreen && fullscreen_window.as_ref() != Some(window) {
            continue;
        }
        let geom_loc = window.geometry().loc;
        let geom_size = window.geometry().size;
        let Some(wl_surface) = window.wl_surface() else {
            continue;
        };
        let is_fullscreen = state.fullscreen.values().any(|fs| &fs.window == window);
        let has_ssd = !is_fullscreen && state.decorations.contains_key(&wl_surface.id());

        let applied = driftwm::config::applied_rule(&wl_surface);
        let is_widget = applied.as_ref().is_some_and(|r| r.widget);
        let is_pinned = state.is_pinned(window);
        let is_focused = focused_surface.as_ref().is_some_and(|f| *f == *wl_surface);
        let effective_mode = driftwm::config::effective_decoration_mode(
            applied.as_ref().and_then(|r| r.decoration.as_ref()),
            &state.config.decorations.default_mode,
        );
        let effective_bw = if is_fullscreen {
            0
        } else {
            driftwm::config::effective_border_width(
                applied.as_ref(),
                effective_mode,
                &state.config.decorations,
            )
        };
        let border_color = if is_focused {
            driftwm::config::effective_border_color_focused(
                applied.as_ref(),
                &state.config.decorations,
            )
        } else {
            driftwm::config::effective_border_color(applied.as_ref(), &state.config.decorations)
        };
        let effective_corner_radius = driftwm::config::effective_corner_radius(
            applied.as_ref(),
            effective_mode,
            &state.config.decorations,
        );
        let effective_shadow = !is_fullscreen
            && driftwm::config::effective_shadow_enabled(
                applied.as_ref(),
                effective_mode,
                &state.config.decorations,
            );

        let mut bbox = window.bbox();
        bbox.loc += loc - geom_loc;
        if has_ssd {
            let r = driftwm::config::DecorationConfig::SHADOW_RADIUS.ceil() as i32;
            let bar = state.config.decorations.title_bar_height;
            bbox.loc.x -= r;
            bbox.loc.y -= bar + r;
            bbox.size.w += 2 * r;
            bbox.size.h += bar + 2 * r;
        }
        if effective_bw > 0 {
            bbox.loc.x -= effective_bw;
            bbox.loc.y -= effective_bw;
            bbox.size.w += 2 * effective_bw;
            bbox.size.h += 2 * effective_bw;
        }
        if !visible_rect.overlaps(bbox) {
            continue;
        }

        // Centralized canvas↔screen decision: pinned windows render at their
        // output-relative `screen_pos` with zoom 1.0 (identity), normal windows
        // use the camera transform. `zoom` is shadowed so every downstream
        // scale (clip, border, shadow, blur, rescale) follows automatically.
        let Some((render_loc, zoom)) =
            state.window_render_transform(window, Some(output), camera, zoom)
        else {
            continue;
        };

        #[cfg(feature = "profile-with-tracy")]
        {
            visible_windows += 1;
            if effective_shadow {
                shadow_elems += 1;
            }
        }
        let client_blur_rects = with_states(&wl_surface, |s| {
            crate::handlers::background_effect::get_cached_blur_region(s)
        });
        // Empty rect list = client explicitly opted out → treat as off.
        let client_blur = client_blur_rects.as_ref().is_some_and(|r| !r.is_empty());
        let wants_blur = blur_enabled && (applied.as_ref().is_some_and(|r| r.blur) || client_blur);
        let opacity = applied.as_ref().and_then(|r| r.opacity).unwrap_or(1.0);

        // Split elements: toplevel + subsurfaces get corner-clipped, popups
        // don't (they can legitimately extend outside the parent's geometry —
        // GTK menus, tooltips, autocomplete, etc). smithay's
        // `Window::render_elements` bundles popups into one vec, which is why
        // we can't use it directly for Wayland.
        let loc_phys: Point<i32, Physical> = render_loc.to_physical_precise_round(scale);
        let (elems, popup_elems) = if let Some(toplevel) = window.toplevel() {
            let root = toplevel.wl_surface();
            let top =
                smithay::backend::renderer::element::surface::render_elements_from_surface_tree::<
                    _,
                    WaylandSurfaceRenderElement<GlesRenderer>,
                >(
                    renderer,
                    root,
                    loc_phys,
                    scale,
                    opacity as f32,
                    Kind::Unspecified,
                );

            let mut popups: Vec<WaylandSurfaceRenderElement<GlesRenderer>> = Vec::new();
            for (popup, popup_offset) in smithay::desktop::PopupManager::popups_for_surface(root) {
                let offset: Point<i32, Physical> = (window.geometry().loc + popup_offset
                    - popup.geometry().loc)
                    .to_physical_precise_round(scale);
                popups.extend(smithay::backend::renderer::element::surface::render_elements_from_surface_tree::<
                    _, WaylandSurfaceRenderElement<GlesRenderer>,
                >(renderer, popup.wl_surface(), loc_phys + offset, scale, opacity as f32, Kind::Unspecified));
            }
            (top, popups)
        } else {
            // No toplevel — render the window's surface tree directly.
            let elems = window.render_elements::<WaylandSurfaceRenderElement<GlesRenderer>>(
                renderer,
                loc_phys,
                scale,
                opacity as f32,
            );
            (elems, Vec::new())
        };

        // Test `is_pinned` BEFORE `is_widget`: a pinned *widget* must land in the
        // pinned bucket (above normal), not `zoomed_widgets` (below normal).
        let target = if is_pinned {
            &mut zoomed_pinned
        } else if is_widget {
            &mut zoomed_widgets
        } else {
            &mut zoomed_normal
        };
        let elem_start = target.len();
        let mut shadow_count = 0usize;

        // Popups push first (earlier in vec = on-top in smithay z-order) so
        // they sit above the title bar and clipped window content.
        push_plain_elements(target, popup_elems, zoom);

        if has_ssd {
            let bar_height = state.config.decorations.title_bar_height;
            // Snap the title-bar band to whole physical pixels and share it across the
            // bar, border, and shadow. Deriving each from the raw logical bar_height
            // rounds their common top edge inconsistently at fractional scale
            // (round(t*s) - round(h*s) != round((t-h)*s)) — a ±1px seam.
            let bar_h_phys = (bar_height as f64 * scale.y).round();
            let bar_h_logical = bar_h_phys / scale.y;

            // Title falls back to app_id, then blank.
            let deco_title = window
                .window_title()
                .or_else(|| window.app_id_or_class())
                .unwrap_or_default();
            if let Some(deco) = state.decorations.get_mut(&wl_surface.id()) {
                deco.update(
                    geom_size.w,
                    is_focused,
                    is_pinned,
                    state.decoration_scale,
                    &deco_title,
                    &state.config.decorations,
                );
            }

            if let Some(deco) = state.decorations.get(&wl_surface.id()) {
                let bar_physical: Point<f64, Physical> =
                    Point::from((loc_phys.x as f64, loc_phys.y as f64 - bar_h_phys));
                let bar_alpha = if opacity < 1.0 {
                    Some(opacity as f32)
                } else {
                    None
                };
                if let Ok(bar_elem) = MemoryRenderBufferRenderElement::from_buffer(
                    renderer,
                    bar_physical,
                    &deco.title_bar,
                    bar_alpha,
                    None,
                    None,
                    Kind::Unspecified,
                ) {
                    target.push(OutputRenderElements::Decoration(
                        PixelSnapRescaleElement::from_element(
                            bar_elem,
                            Point::<i32, Physical>::from((0, 0)),
                            zoom,
                        ),
                    ));
                }
            }

            // Only bottom corners round (title bar covers the top edge).
            if let Some(ref shader) = state.render.corner_clip_shader {
                let radius = effective_corner_radius as f32;
                if radius > 0.0 {
                    let wg = window.geometry();
                    let geometry = Rectangle::new(
                        Point::<f64, Logical>::from((
                            render_loc.x + wg.loc.x as f64,
                            render_loc.y + wg.loc.y as f64,
                        )),
                        Size::<f64, Logical>::from((wg.size.w as f64, wg.size.h as f64)),
                    );
                    push_corner_clipped_elements(
                        target,
                        elems,
                        shader,
                        geometry,
                        [0.0, 0.0, radius, radius],
                        zoom,
                        output_scale,
                    );
                } else {
                    push_plain_elements(target, elems, zoom);
                }
            } else {
                push_plain_elements(target, elems, zoom);
            }

            // Border wraps title bar + content; drawn between window content
            // and shadow so it sits outside the rounded corner mask.
            if effective_bw > 0
                && let Some(shader) = state.render.border_shader.clone()
            {
                let inner_logical: Rectangle<f64, Logical> = Rectangle::new(
                    (render_loc.x, render_loc.y - bar_h_logical).into(),
                    (geom_size.w as f64, geom_size.h as f64 + bar_h_logical).into(),
                );
                push_border_element(
                    target,
                    &mut state.render.border_cache,
                    wl_surface.id(),
                    &shader,
                    inner_logical,
                    effective_corner_radius as f32,
                    effective_bw,
                    border_color,
                    is_focused,
                    opacity,
                    scale,
                    zoom,
                );
            }

            // Shadow encloses title bar + content + border; cached per-surface
            // so the damage tracker can skip unchanged regions. With a border,
            // footprint and corner radius grow by border_width so the shadow
            // grades from the border's outer perimeter (matches niri).
            if effective_shadow && let Some(shader) = state.render.shadow_shader.clone() {
                let bw = effective_bw as f64;
                let body_logical: Rectangle<f64, Logical> = Rectangle::new(
                    (render_loc.x - bw, render_loc.y - bar_h_logical - bw).into(),
                    (
                        geom_size.w as f64 + 2.0 * bw,
                        geom_size.h as f64 + bar_h_logical + 2.0 * bw,
                    )
                        .into(),
                );
                push_shadow_element(
                    target,
                    &mut state.render.shadow_cache,
                    wl_surface.id(),
                    &shader,
                    body_logical,
                    (effective_corner_radius + effective_bw) as f32,
                    opacity,
                    scale,
                    zoom,
                );
                shadow_count = 1;
            }
        } else if let Some(ref shader) = state.render.corner_clip_shader {
            let geo = window.geometry();
            let radius = effective_corner_radius as f32;

            // `decoration = "none"` hard-vetoes compositor chrome: the client
            // surface is passed through untouched (no clip, no border, no
            // shadow). Use `minimal` for titlebar-less chrome opt-ins.
            let effective = driftwm::config::effective_decoration_mode(
                applied.as_ref().and_then(|r| r.decoration.as_ref()),
                &state.config.decorations.default_mode,
            );
            let bare = matches!(effective, driftwm::config::DecorationMode::None);

            if !bare && !is_fullscreen {
                // Clip pixels outside the geometry rect even when radius=0,
                // so a CSD client's own shadow (drawn in a subsurface beyond
                // geometry) doesn't stack under our compositor shadow and
                // double it up.
                let geometry = Rectangle::new(
                    Point::<f64, Logical>::from((
                        render_loc.x + geo.loc.x as f64,
                        render_loc.y + geo.loc.y as f64,
                    )),
                    Size::<f64, Logical>::from((geo.size.w as f64, geo.size.h as f64)),
                );
                push_corner_clipped_elements(
                    target,
                    elems,
                    shader,
                    geometry,
                    [radius, radius, radius, radius],
                    zoom,
                    output_scale,
                );

                if effective_bw > 0
                    && let Some(border_shader) = state.render.border_shader.clone()
                {
                    push_border_element(
                        target,
                        &mut state.render.border_cache,
                        wl_surface.id(),
                        &border_shader,
                        geometry,
                        radius,
                        effective_bw,
                        border_color,
                        is_focused,
                        opacity,
                        scale,
                        zoom,
                    );
                }

                // Footprint and corner radius grow by border_width so the
                // shadow grades from the border's outer edge, not the content edge.
                if effective_shadow && let Some(shader) = state.render.shadow_shader.clone() {
                    let bw = effective_bw as f64;
                    let body_logical: Rectangle<f64, Logical> = Rectangle::new(
                        (
                            render_loc.x + geo.loc.x as f64 - bw,
                            render_loc.y + geo.loc.y as f64 - bw,
                        )
                            .into(),
                        (geom_size.w as f64 + 2.0 * bw, geom_size.h as f64 + 2.0 * bw).into(),
                    );
                    push_shadow_element(
                        target,
                        &mut state.render.shadow_cache,
                        wl_surface.id(),
                        &shader,
                        body_logical,
                        (effective_corner_radius + effective_bw) as f32,
                        opacity,
                        scale,
                        zoom,
                    );
                    shadow_count = 1;
                }
            } else {
                // Bare (`decoration = "none"`) or fullscreen: pass through.
                push_plain_elements(target, elems, zoom);
            }
        } else {
            push_plain_elements(target, elems, zoom);
        }

        if wants_blur && (target.len() - elem_start - shadow_count) > 0 {
            let elem_count = target.len() - elem_start - shadow_count;
            let screen_loc: Point<i32, Logical> =
                Point::from(((render_loc.x * zoom) as i32, (render_loc.y * zoom) as i32));
            let screen_size: Size<i32, Logical> = if has_ssd {
                let bar = state.config.decorations.title_bar_height;
                (
                    (geom_size.w as f64 * zoom).ceil() as i32,
                    ((geom_size.h + bar) as f64 * zoom).ceil() as i32,
                )
                    .into()
            } else {
                (
                    (geom_size.w as f64 * zoom).ceil() as i32,
                    (geom_size.h as f64 * zoom).ceil() as i32,
                )
                    .into()
            };
            let screen_rect = Rectangle::new(
                if has_ssd {
                    Point::from((
                        screen_loc.x,
                        screen_loc.y
                            - (state.config.decorations.title_bar_height as f64 * zoom) as i32,
                    ))
                } else {
                    // CSD: geometry starts at render_loc + geo.loc.
                    let geo = window.geometry();
                    Point::from((
                        ((render_loc.x + geo.loc.x as f64) * zoom) as i32,
                        ((render_loc.y + geo.loc.y as f64) * zoom) as i32,
                    ))
                },
                screen_size,
            )
            .to_physical_precise_round(output_scale);

            // Convert client blur region: surface-local Logical → mask-local
            // Physical at composite_scale = zoom × output_scale.
            // wl_surface (0,0) offset within mask:
            //   SSD: (0, TITLE_BAR_HEIGHT) — title bar shifts mask up.
            //   CSD: -geo.loc — screen_rect anchored at geometry, not surface.
            let region_rects = if client_blur {
                let rects = client_blur_rects.as_ref().unwrap();
                let composite_scale = zoom * output_scale;
                let (offset_x, offset_y): (f64, f64) = if has_ssd {
                    (0.0, state.config.decorations.title_bar_height as f64)
                } else {
                    let geo = window.geometry();
                    (-geo.loc.x as f64, -geo.loc.y as f64)
                };
                let win_bounds: Rectangle<i32, Physical> = Rectangle::from_size(screen_rect.size);
                let mut out: Vec<Rectangle<i32, Physical>> = Vec::with_capacity(rects.len());
                for r in rects.iter() {
                    let x1 = ((r.loc.x as f64 + offset_x) * composite_scale).round() as i32;
                    let y1 = ((r.loc.y as f64 + offset_y) * composite_scale).round() as i32;
                    let x2 =
                        (((r.loc.x + r.size.w) as f64 + offset_x) * composite_scale).round() as i32;
                    let y2 =
                        (((r.loc.y + r.size.h) as f64 + offset_y) * composite_scale).round() as i32;
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

            // If all client rects clipped to nothing AND no rule asked for
            // blur, skip — otherwise region_rects=None would be interpreted
            // as whole-window blur, against what the client requested.
            let rule_blur = applied.as_ref().is_some_and(|r| r.blur);
            let skip_clipped_out = client_blur && region_rects.is_none() && !rule_blur;

            if !skip_clipped_out {
                blur_requests.push(BlurRequestData {
                    surface_id: wl_surface.id(),
                    screen_rect,
                    elem_start,
                    elem_count,
                    layer: if is_pinned {
                        BlurLayer::Pinned
                    } else if is_widget {
                        BlurLayer::Widget
                    } else {
                        BlurLayer::Normal
                    },
                    region_rects,
                });
            }
        }
    }

    #[cfg(feature = "profile-with-tracy")]
    {
        static VISIBLE_PLOT: std::sync::OnceLock<tracy_client::PlotName> =
            std::sync::OnceLock::new();
        static SHADOW_PLOT: std::sync::OnceLock<tracy_client::PlotName> =
            std::sync::OnceLock::new();
        static CAMERA_X_PLOT: std::sync::OnceLock<tracy_client::PlotName> =
            std::sync::OnceLock::new();
        static CAMERA_Y_PLOT: std::sync::OnceLock<tracy_client::PlotName> =
            std::sync::OnceLock::new();
        let visible = VISIBLE_PLOT
            .get_or_init(|| tracy_client::PlotName::new_leak("frame.visible_windows".to_string()));
        let shadows = SHADOW_PLOT
            .get_or_init(|| tracy_client::PlotName::new_leak("frame.shadow_elems".to_string()));
        // Camera position per composed frame: per-frame deltas of this measure
        // motion uniformity (judder), independent of frame cadence.
        let cam_x = CAMERA_X_PLOT
            .get_or_init(|| tracy_client::PlotName::new_leak("frame.camera_x".to_string()));
        let cam_y = CAMERA_Y_PLOT
            .get_or_init(|| tracy_client::PlotName::new_leak("frame.camera_y".to_string()));
        if let Some(client) = tracy_client::Client::running() {
            client.plot(*visible, visible_windows as f64);
            client.plot(*shadows, shadow_elems as f64);
            client.plot(*cam_x, camera.x);
            client.plot(*cam_y, camera.y);
        }
    }

    #[cfg(feature = "profile-with-tracy")]
    drop(_windows_span);

    // Both sit below the windows, so the fullscreen window fully occludes them.
    let canvas_layer_elements = if output_fullscreen {
        Vec::new()
    } else {
        build_canvas_layer_elements(state, renderer, output_scale, camera, zoom, visible_rect)
    };

    let outline_elements = if output_fullscreen {
        Vec::new()
    } else {
        build_output_outline_elements(state, renderer, output, camera, zoom, viewport_size)
    };

    let bg_elements: Vec<OutputRenderElements> = if output_fullscreen {
        vec![]
    } else if let Some(cache) = state.render.cached_shader_chunks.get_mut(&output.name()) {
        cache
            .render_elements(visible_rect, renderer, camera, zoom)
            .into_iter()
            .map(OutputRenderElements::TileBgChunk)
            .collect()
    } else if let Some(cache) = state.render.cached_tile_chunks.get_mut(&output.name()) {
        // 8 GLES uploads/frame: decode is off-thread, so render-time per
        // blob is the only constraint. import_memory of a 256×256 RGBA8 is
        // sub-ms on M1, ~2-3ms on weak iGPUs — 8 keeps upload under ~25ms on
        // the slow path and drains a worker burst in one frame on fast
        // hardware. Coarser-LOD overlays + fallback plane cover undrained.
        cache.ensure_visible_loaded(visible_rect, renderer, zoom, 8);
        tile_chunks::chunk_render_elements(cache, visible_rect, camera, zoom)
            .into_iter()
            .map(OutputRenderElements::TileBgChunk)
            .collect()
    } else if let Some(bg) = state.render.cached_bg.get(&output.name()) {
        bg.render_element(zoom).into_iter().collect()
    } else {
        vec![]
    };

    let is_fullscreen = state.is_output_fullscreen(output);
    #[cfg(feature = "profile-with-tracy")]
    let _layers_span = tracy_client::span!("compose::layers");
    let (overlay_elements, overlay_blur) = build_layer_elements(
        state,
        output,
        renderer,
        WlrLayer::Overlay,
        Some(BlurLayer::Overlay),
    );
    let (top_elements, top_blur) = if !is_fullscreen {
        build_layer_elements(state, output, renderer, WlrLayer::Top, Some(BlurLayer::Top))
    } else {
        (vec![], vec![])
    };
    let (bottom_elements, _) = if !is_fullscreen {
        build_layer_elements(state, output, renderer, WlrLayer::Bottom, None)
    } else {
        (vec![], vec![])
    };
    let (background_layer_elements, _) = if !is_fullscreen {
        build_layer_elements(state, output, renderer, WlrLayer::Background, None)
    } else {
        (vec![], vec![])
    };
    #[cfg(feature = "profile-with-tracy")]
    drop(_layers_span);

    // Prefix offsets locate each group in all_elements for blur insertion.
    let overlay_prefix = cursor_elements.len();
    let top_prefix = overlay_prefix + overlay_elements.len();
    let pinned_prefix = top_prefix + top_elements.len();
    let normal_prefix = pinned_prefix + zoomed_pinned.len();
    let widget_prefix = normal_prefix + zoomed_normal.len() + canvas_layer_elements.len();

    // Layer surfaces first (front-to-back), then windows.
    let mut all_blur_requests: Vec<BlurRequestData> = Vec::new();
    all_blur_requests.extend(overlay_blur);
    all_blur_requests.extend(top_blur);
    all_blur_requests.extend(blur_requests);

    let mut all_elements: Vec<OutputRenderElements> = Vec::with_capacity(
        cursor_elements.len()
            + overlay_elements.len()
            + top_elements.len()
            + zoomed_pinned.len()
            + zoomed_normal.len()
            + canvas_layer_elements.len()
            + zoomed_widgets.len()
            + bottom_elements.len()
            + outline_elements.len()
            + bg_elements.len()
            + background_layer_elements.len(),
    );
    let cursor_count = cursor_elements.len();
    // Everything from bottom_elements down is scene background for the
    // shared animated blur (below all windows and widgets).
    let background_suffix = bottom_elements.len()
        + outline_elements.len()
        + bg_elements.len()
        + background_layer_elements.len();
    all_elements.extend(cursor_elements);
    all_elements.extend(overlay_elements);
    all_elements.extend(top_elements);
    all_elements.extend(zoomed_pinned);
    all_elements.extend(zoomed_normal);
    all_elements.extend(canvas_layer_elements);
    all_elements.extend(zoomed_widgets);
    all_elements.extend(bottom_elements);
    all_elements.extend(outline_elements);
    all_elements.extend(bg_elements);
    all_elements.extend(background_layer_elements);
    let background_start = all_elements.len() - background_suffix;

    if !all_blur_requests.is_empty() {
        #[cfg(feature = "profile-with-tracy")]
        let _blur_span = tracy_client::span!("compose::blur");
        process_blur_requests(
            state,
            renderer,
            output,
            output_scale,
            &mut all_elements,
            &all_blur_requests,
            overlay_prefix,
            top_prefix,
            pinned_prefix,
            normal_prefix,
            widget_prefix,
            background_start,
        );
    }

    if blur_enabled {
        let active_ids: std::collections::HashSet<_> = all_blur_requests
            .iter()
            .map(|r| r.surface_id.clone())
            .collect();
        state
            .render
            .blur_cache
            .retain(|id, _| active_ids.contains(id));
    }

    // Error bar sits above every window and layer-shell surface but below the
    // cursor. Spliced after blur so it doesn't shift the prefix offsets blur
    // indexes into.
    let error_bar = error_bar::build_error_bar_elements(state, renderer, output);
    if !error_bar.is_empty() {
        all_elements.splice(cursor_count..cursor_count, error_bar);
    }

    all_elements
}

/// Thin outlines showing where other monitors' viewports sit on the canvas.
fn build_output_outline_elements(
    state: &crate::state::DriftWm,
    renderer: &mut GlesRenderer,
    output: &Output,
    camera: Point<f64, Logical>,
    zoom: f64,
    viewport_size: Size<i32, Logical>,
) -> Vec<OutputRenderElements> {
    let thickness = state.config.output_outline.thickness;
    if thickness <= 0 {
        return vec![];
    }

    let opacity = state.config.output_outline.opacity as f32;
    if opacity <= 0.0 {
        return vec![];
    }
    let color = state.config.output_outline.color;
    let scale = output.current_scale().fractional_scale();

    let mut elements = Vec::new();

    for other in state.space.outputs() {
        if *other == *output {
            continue;
        }
        // A fullscreen output shows a screen-space window, not a canvas
        // viewport, so it has no outline to project onto other monitors.
        if state.is_output_fullscreen(other) {
            continue;
        }

        let (other_camera, other_zoom) = {
            let os = crate::state::output_state(other);
            (os.camera, os.zoom)
        };
        let other_size = crate::state::output_logical_size(other);

        let other_canvas =
            canvas::visible_canvas_rect(other_camera.to_i32_round(), other_size, other_zoom);

        // Transform to screen coords on *this* output.
        let screen_x = ((other_canvas.loc.x as f64 - camera.x) * zoom) as i32;
        let screen_y = ((other_canvas.loc.y as f64 - camera.y) * zoom) as i32;
        let screen_w = (other_canvas.size.w as f64 * zoom) as i32;
        let screen_h = (other_canvas.size.h as f64 * zoom) as i32;

        let vp = Rectangle::from_size(viewport_size);
        let outline_rect = Rectangle::new((screen_x, screen_y).into(), (screen_w, screen_h).into());
        if !vp.overlaps(outline_rect) {
            continue;
        }

        let edges: [(i32, i32, i32, i32); 4] = [
            (screen_x, screen_y, screen_w, thickness), // top
            (
                screen_x,
                screen_y + screen_h - thickness,
                screen_w,
                thickness,
            ), // bottom
            (screen_x, screen_y, thickness, screen_h), // left
            (
                screen_x + screen_w - thickness,
                screen_y,
                thickness,
                screen_h,
            ), // right
        ];

        for (ex, ey, ew, eh) in edges {
            let x0 = ex.max(0);
            let y0 = ey.max(0);
            let x1 = (ex + ew).min(viewport_size.w);
            let y1 = (ey + eh).min(viewport_size.h);
            if x1 <= x0 || y1 <= y0 {
                continue;
            }

            let w = x1 - x0;
            let h = y1 - y0;

            let pixels: Vec<u8> = vec![color[0], color[1], color[2], color[3]]
                .into_iter()
                .cycle()
                .take((w * h) as usize * 4)
                .collect();

            let buf = MemoryRenderBuffer::from_slice(
                &pixels,
                Fourcc::Abgr8888,
                (w, h),
                1,
                Transform::Normal,
                None,
            );

            let loc: Point<f64, Physical> = Point::from((x0, y0)).to_f64().to_physical(scale);
            if let Ok(elem) = MemoryRenderBufferRenderElement::from_buffer(
                renderer,
                loc,
                &buf,
                Some(opacity),
                None,
                None,
                Kind::Unspecified,
            ) {
                elements.push(OutputRenderElements::Decoration(
                    PixelSnapRescaleElement::from_element(
                        elem,
                        Point::<i32, Physical>::from((0, 0)),
                        1.0,
                    ),
                ));
            }
        }
    }

    elements
}
