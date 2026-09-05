mod background;
mod blur;
mod capture;
mod capture_background;
mod chrome;
mod closing;
mod cursor;
mod elements;
mod error_bar;
mod layers;
mod lifecycle;
mod screenshot;
mod shader_chunks;
mod shaders;
mod suspended;
mod tile_chunks;
mod tile_chunks_tiff;
mod tile_worker;

pub use background::{BackgroundElement, init_background, update_background_element};
pub(crate) use blur::compile_blur_shaders;
pub use blur::{BlurCache, BlurScratchPool, SharedBlur};
pub use capture::{render_capture_frames, render_screencopy, render_toplevel_captures};
pub(crate) use closing::{
    BakeChrome, CloseChrome, ClosePixels, ClosingSnapshot, ResizeCaptures, ResizeCrossfade,
    StandInFade, capture_close_pixels, close_pixels_fresh, resize_crossfade, snapshot_canvas,
    snapshot_screen,
};
pub use cursor::build_cursor_elements;
pub use elements::{
    OutputRenderElements, PixelSnapRescaleElement, RoundedCornerElement, TileShaderElement,
    TrimmedElement, WindowTransformElement, painted_rect, painted_size,
};
pub use error_bar::ErrorBarCache;
pub use lifecycle::{
    post_render, refresh_ext_workspaces, refresh_foreign_toplevels, send_frame_callbacks_fallback,
    take_presentation_feedback, update_primary_scanout_output,
};
pub use screenshot::capture_region_to_png;
pub use shader_chunks::ShaderChunkCache;
pub use shaders::{
    BorderPhysKey, ShadowPhysKey, compile_border_shader, compile_corner_clip_shader,
    compile_shadow_shader,
};
pub use tile_chunks::BgChunkCache;

#[cfg(test)]
pub(crate) use suspended::{ensure_body, ensure_label};

use blur::{BlurLayer, BlurRequestData, process_blur_requests};
use chrome::DrawnChrome;
use layers::{build_canvas_layer_elements, build_layer_elements};
use shaders::{outer_corner_radius, push_border_element, push_shadow_element};

/// The per-window affine transform for an in-flight open/close/geometry
/// animation, threaded through the chrome push helpers so the surface, border,
/// shadow, and decoration all lerp together. Physical `origin`/`offset`; the
/// `scale` is the visual stretch relative to the live rect.
#[derive(Clone, Copy)]
pub(super) struct WindowRenderAnimation {
    origin: Point<f64, Physical>,
    offset: Point<f64, Physical>,
    scale: Scale<f64>,
}

/// The whole extent of a texture we rasterized ourselves, as the `src` a
/// `TextureRenderElement` wants.
///
/// Our offscreens hold `logical * scale` texels but are wrapped at buffer scale
/// 1, so for them one "logical" unit *is* one texel and `src` has to be given in
/// texels. Leaving `src` at `None` makes smithay fall back to the element's
/// logical size — the *destination* extent — which on a HiDPI or zoomed-in
/// surface samples only the top-left `1/scale`-squared of the texture and
/// stretches it over the whole destination.
pub(super) fn texel_src(texels: Size<i32, Physical>) -> Rectangle<f64, Logical> {
    Rectangle::from_size(Size::from((texels.w as f64, texels.h as f64)))
}

/// One adoption crossfade's render inputs, lifted out of `state` before the
/// per-fade mutable borrows. `focused`/`launching` are frozen at fade creation.
struct FadeRender {
    suspended: std::rc::Rc<crate::state::SuspendedWindow>,
    loc: Point<i32, Logical>,
    focused: bool,
    launching: bool,
    alpha: f32,
    /// `None` unless the fade actually shrinks — an identity transform would
    /// still change the element variant and drop its opaque regions, so the
    /// adoption crossfade must keep taking the untransformed path.
    animation: Option<WindowRenderAnimation>,
}

impl WindowRenderAnimation {
    /// Apply the same affine the element decorator applies, so the frost rect
    /// follows the animated window instead of its (instant) logical position.
    fn transform_phys_rect(&self, rect: Rectangle<i32, Physical>) -> Rectangle<i32, Physical> {
        let x0 = self.origin.x + (rect.loc.x as f64 - self.origin.x) * self.scale.x + self.offset.x;
        let y0 = self.origin.y + (rect.loc.y as f64 - self.origin.y) * self.scale.y + self.offset.y;
        let x1 = self.origin.x
            + ((rect.loc.x + rect.size.w) as f64 - self.origin.x) * self.scale.x
            + self.offset.x;
        let y1 = self.origin.y
            + ((rect.loc.y + rect.size.h) as f64 - self.origin.y) * self.scale.y
            + self.offset.y;
        // Round each corner independently, matching `WindowTransformElement`'s
        // own rounding, so the frost rect and the element rect can't disagree by
        // a pixel.
        Rectangle::new(
            Point::from((x0.round() as i32, y0.round() as i32)),
            Size::from((
                (x1.round() as i32 - x0.round() as i32).max(0),
                (y1.round() as i32 - y0.round() as i32).max(0),
            )),
        )
    }
}

use std::collections::HashMap;

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

use crate::decorations::DecorationKey;
use crate::state::StageWindow;
use driftwm::canvas;
use driftwm::window_ext::WindowExt;

/// Render elements for a locked session: the lock surface, with the cursor
/// over it. A lock client's `wl_pointer.set_cursor` arrives as a
/// `CursorImageStatus` we composite ourselves, exactly as for any other
/// client — nothing draws a cursor here but us.
fn compose_lock_frame(
    state: &crate::state::DriftWm,
    renderer: &mut GlesRenderer,
    output: &Output,
    cursor_elements: Vec<OutputRenderElements>,
) -> Vec<OutputRenderElements> {
    // Cursor first, as in `compose_frame`, so it draws topmost.
    let mut elements = cursor_elements;

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
    animation: Option<WindowRenderAnimation>,
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
        let elem = PixelSnapRescaleElement::from_element(
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
        );
        if let Some(animation) = animation {
            target.push(OutputRenderElements::AnimatedCsdWindow(
                WindowTransformElement::new(
                    elem,
                    animation.origin,
                    animation.offset,
                    animation.scale,
                ),
            ));
        } else {
            target.push(OutputRenderElements::CsdWindow(elem));
        }
    }
}

fn push_plain_elements(
    target: &mut Vec<OutputRenderElements>,
    elems: Vec<WaylandSurfaceRenderElement<GlesRenderer>>,
    zoom: f64,
    animation: Option<WindowRenderAnimation>,
) {
    target.extend(elems.into_iter().map(|elem| {
        let elem =
            PixelSnapRescaleElement::from_element(elem, Point::<i32, Physical>::from((0, 0)), zoom);
        if let Some(animation) = animation {
            OutputRenderElements::AnimatedWindow(WindowTransformElement::new(
                elem,
                animation.origin,
                animation.offset,
                animation.scale,
            ))
        } else {
            OutputRenderElements::Window(elem)
        }
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
///
/// When `isolate` is `Some(element)`, only that element (a client window plus
/// its popups + chrome, or a suspended stand-in's chrome) is composed, so
/// overlapping neighbors never leak in. A client renders at its stage position
/// regardless of kind (see the render-loc note below), which is what lets a
/// `window` capture cover pinned and fullscreen windows too.
pub(crate) fn compose_capture_elements(
    state: &mut crate::state::DriftWm,
    renderer: &mut GlesRenderer,
    camera: Point<f64, Logical>,
    dpi_scale: f64,
    viewport_logical: Size<i32, Logical>,
    capture_bg: &capture_background::CaptureBackground,
    isolate: Option<&crate::state::StageWindow>,
) -> Vec<OutputRenderElements> {
    use smithay::backend::renderer::element::surface::render_elements_from_surface_tree;

    let zoom = 1.0;
    let output_scale = dpi_scale;
    let scale = Scale::from(dpi_scale);
    let visible_rect = canvas::visible_canvas_rect(camera.to_i32_round(), viewport_logical, zoom);

    let focused_window = state.focus_root_window();

    let mut normal: Vec<OutputRenderElements> = Vec::new();
    let mut widgets: Vec<OutputRenderElements> = Vec::new();

    // Collect first: the surface-tree calls borrow `state`, which would conflict
    // with an in-flight `state.stage.windows()` iterator. Elements are ref-counted.
    let elements: Vec<StageWindow> = state.stage.windows().rev().cloned().collect();
    for element in &elements {
        // Isolation (`msg screenshot window`) composes only its target — a
        // client or a suspended stand-in. Whole-canvas / `all` captures pass
        // `None` and render every element.
        if isolate.is_some_and(|target| target != element) {
            continue;
        }
        let window = match element {
            StageWindow::Client(w) => w,
            StageWindow::Suspended(s) => {
                let Some(loc) = state.stage.position_of(element) else {
                    continue;
                };
                let focused = state.gated_suspended_focus() == Some(s.id);
                let launching = state.is_suspended_launching(s.id);
                let border_shader = state.render.border_shader.clone();
                let shadow_shader = state.render.shadow_shader.clone();
                suspended::push_suspended_element(
                    renderer,
                    s,
                    loc,
                    focused,
                    launching,
                    1.0,
                    // Captures and screenshots want the settled frame, never a
                    // mid-slide one — deliberately un-animated, like the client
                    // arm below.
                    None,
                    &state.config.decorations,
                    state.decoration_scale,
                    &mut state.decorations,
                    &mut state.render.border_cache,
                    &mut state.render.shadow_cache,
                    border_shader.as_ref(),
                    shadow_shader.as_ref(),
                    camera,
                    zoom,
                    scale,
                    &mut normal,
                );
                continue;
            }
        };
        let Some(loc) = state.stage.position_of(window) else {
            continue;
        };
        let geom_loc = window.geometry().loc;
        let geom_size = window.geometry().size;
        let Some(wl_surface) = window.wl_surface() else {
            continue;
        };
        let is_fullscreen = state.stage.is_fullscreen(window);

        let applied = driftwm::config::applied_rule(&wl_surface);
        let is_widget = applied.as_ref().is_some_and(|r| r.widget);
        let is_focused = focused_window.as_ref() == Some(window);
        // An isolated window is captured as itself, not as the unfocused window
        // it is while the command runs.
        let opacity = if isolate.is_some() {
            applied.as_ref().and_then(|r| r.opacity).unwrap_or(1.0)
        } else {
            driftwm::config::effective_opacity(
                applied.as_ref(),
                &state.config.decorations,
                is_focused,
                is_fullscreen,
            )
        };

        let effective_mode = driftwm::config::effective_decoration_mode(
            applied.as_ref().and_then(|r| r.decoration.as_ref()),
            &state.config.decorations.default_mode,
        );
        let configured = DrawnChrome {
            ssd_bar: state
                .decorations
                .contains_key(&DecorationKey::Surface(wl_surface.id())),
            border_width: driftwm::config::effective_border_width(
                applied.as_ref(),
                effective_mode,
                &state.config.decorations,
            ),
            corner_radius: driftwm::config::effective_corner_radius(
                applied.as_ref(),
                effective_mode,
                &state.config.decorations,
            ),
            shadow: driftwm::config::effective_shadow_enabled(
                applied.as_ref(),
                effective_mode,
                &state.config.decorations,
            ),
        };
        // No output here, so there is no usable area a window could cover.
        let chrome = configured.drawn(is_fullscreen, canvas::Coverage::None);
        let has_ssd = chrome.ssd_bar;
        let effective_bw = chrome.border_width;
        let effective_corner_radius = chrome.corner_radius;
        let effective_shadow = chrome.shadow;
        let border_color = if is_focused {
            driftwm::config::effective_border_color_focused(
                applied.as_ref(),
                &state.config.decorations,
            )
        } else {
            driftwm::config::effective_border_color(applied.as_ref(), &state.config.decorations)
        };

        let mut bbox = window.bbox_with_popups();
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
        //
        // Isolation bypasses that: `window_render_transform` excludes
        // pinned/fullscreen from off-screen renders by design, but here the
        // target renders at its real stage position regardless of kind — the
        // same position the capture region was derived from, so the two agree.
        let render_loc = if isolate.is_some() {
            crate::state::canvas_render_loc(loc, geom_loc, camera)
        } else {
            let Some((render_loc, _)) = state.window_render_transform(window, None, camera, zoom)
            else {
                continue;
            };
            render_loc
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
        push_plain_elements(target, popup_elems, zoom, None);

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
            if let Some(deco) = state
                .decorations
                .get(&DecorationKey::Surface(wl_surface.id()))
            {
                let bar_physical: Point<f64, Physical> =
                    Point::from((loc_phys.x as f64, loc_phys.y as f64 - bar_h_phys));
                let bar_alpha = if opacity < 1.0 {
                    Some(opacity as f32)
                } else {
                    None
                };
                let bar_dst = Rectangle::new(
                    Point::from((loc_phys.x, loc_phys.y - bar_h_phys as i32)),
                    painted_size(
                        Size::<f64, Logical>::from((geom_size.w as f64, bar_h_logical)),
                        scale,
                    ),
                );
                if let Ok(bar_elem) = MemoryRenderBufferRenderElement::from_buffer(
                    renderer,
                    bar_physical,
                    &deco.title_bar,
                    bar_alpha,
                    None,
                    None,
                    Kind::Unspecified,
                ) {
                    target.push(OutputRenderElements::TrimmedDecoration(
                        PixelSnapRescaleElement::from_element(
                            TrimmedElement::from_element(bar_elem, bar_dst),
                            Point::<i32, Physical>::from((0, 0)),
                            zoom,
                        ),
                    ));
                }
            }

            // Only bottom corners round (title bar covers the top edge).
            if let Some(ref shader) = state.render.corner_clip_shader {
                let radius = effective_corner_radius as f32;
                if effective_bw > 0 || radius > 0.0 {
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
                        None,
                    );
                } else {
                    push_plain_elements(target, elems, zoom, None);
                }
            } else {
                push_plain_elements(target, elems, zoom, None);
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
                    wl_surface.id().into(),
                    &shader,
                    inner_logical,
                    effective_corner_radius as f32,
                    effective_bw,
                    border_color,
                    is_focused,
                    opacity,
                    scale,
                    zoom,
                    None,
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
                    wl_surface.id().into(),
                    &shader,
                    body_logical,
                    outer_corner_radius(effective_corner_radius as f32, effective_bw as f32),
                    opacity,
                    scale,
                    zoom,
                    None,
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
                    None,
                );

                if effective_bw > 0
                    && let Some(border_shader) = state.render.border_shader.clone()
                {
                    push_border_element(
                        target,
                        &mut state.render.border_cache,
                        wl_surface.id().into(),
                        &border_shader,
                        geometry,
                        radius,
                        effective_bw,
                        border_color,
                        is_focused,
                        opacity,
                        scale,
                        zoom,
                        None,
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
                        wl_surface.id().into(),
                        &shader,
                        body_logical,
                        outer_corner_radius(effective_corner_radius as f32, effective_bw as f32),
                        opacity,
                        scale,
                        zoom,
                        None,
                    );
                }
            } else {
                push_plain_elements(target, elems, zoom, None);
            }
        } else {
            push_plain_elements(target, elems, zoom, None);
        }
    }

    // Canvas-positioned layer widgets sit between normal windows and widget
    // toplevels, as in compose_frame. Screen-anchored layer surfaces (panels) are
    // excluded — they aren't canvas content. Isolated captures skip them too.
    let canvas_layers = if isolate.is_some() {
        Vec::new()
    } else {
        build_canvas_layer_elements(state, renderer, output_scale, camera, zoom, visible_rect)
    };
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

    if state.session_lock.renders_lock_frame() {
        // No blur draws in a lock frame, and the eviction sweep that frees it
        // sits past this return — so a lock would otherwise hold every blurred
        // window's textures and the shared backdrop for its whole duration,
        // which can be hours. Per output, like the sweep: this frame speaks
        // only for its own output. The re-warm on unlock is the first-frame
        // staging `force_dirty_frames` already covers.
        state.render.remove_blur_caches(&output.name());
        return compose_lock_frame(state, renderer, output, cursor_elements);
    }

    let name = output.name();
    let output_fullscreen = state.is_output_visually_fullscreen(output);
    // The fullscreen picture fully occludes its output: only what draws at or
    // above it, the overlay layer, and the cursor render; everything beneath is
    // culled below. Pinned windows count as top-tier toplevels and get covered
    // like the top layer. Resolved visually, so an exit still frozen on the
    // fullscreen picture keeps showing that picture instead of being culled
    // along with everything else — and so does a window growing into the
    // fullscreen that exit is handing over.
    let fullscreen_windows = if output_fullscreen {
        state.visually_fullscreen_windows_on(output)
    } else {
        Vec::new()
    };
    // The canvas alone answers to a second predicate: a fullscreen window drawn
    // translucent by its rule `opacity` still covers the output, but you see the
    // plane through it, so the background, the canvas layers and the outlines
    // stay drawn under it. Everything else keeps culling on coverage.
    let fullscreen_conceals = state.fullscreen_conceals_canvas(output);
    let mut did_init_bg = false;
    if fullscreen_conceals {
        // A concealing fullscreen picture hides the canvas: free the bulk of its
        // chunk caches and skip the background. Shrunk rather than removed, so
        // the caches stay in their maps and the frame that uncovers the canvas —
        // an exit, or an opacity drop — has no synchronous rebuild to pay for.
        // Maximize is NOT fullscreen, so it keeps its background.
        state.render.shrink_background_for_fullscreen(&name);
    } else if !state.render.cached_bg.contains_key(&name)
        && !state.render.cached_tile_chunks.contains_key(&name)
        && !state.render.cached_shader_chunks.contains_key(&name)
    {
        // Reachable while fullscreen, when the window is translucent: anything
        // that drops the caches outright (config reload, scale or transform
        // change — see `remove_background_chunks`) rebuilds them here, inside
        // the fullscreen frame, instead of deferring the cost to the exit frame.
        // For a gigapixel TIFF that is a whole-LOD decode and six thread spawns
        // on a frame the user is watching a fullscreen window in. Inherent to
        // showing the canvas through that window, not a pacing regression.
        let output_size = crate::state::output_logical_size(output);
        init_background(state, renderer, output_size, &name);
        did_init_bg = true;
    }

    // Read per-output state directly — active_output() follows the pointer,
    // which is wrong when rendering an output the pointer isn't on.
    let (live_camera, live_zoom) = {
        let os = crate::state::output_state(output);
        (os.camera, os.zoom)
    };
    // Entering fullscreen parks the viewport at zoom 1 in one step, but the
    // window only covers the output at the end of its growth — so for those few
    // frames the whole scene behind it would pop to the parked view. Draw the
    // world through the pre-fullscreen view instead: it is culled outright the
    // moment the leg lands, so it never has to travel anywhere and the park stays
    // the implementation detail it is. Only the entering window itself reads the
    // live view below, since that is the frame its growth was seeded in.
    let entering_fullscreen = state.fullscreen_entry_on(output);
    let (camera, zoom) = state.world_view(output);
    // The rect every window's destination frame is tested against below.
    let usable = state.usable_area_on(output);

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
    // Closing snapshots + adoption fades: their own bucket above normal windows
    // so they never shift the normal windows' blur element indices.
    let mut zoomed_closing: Vec<OutputRenderElements> = Vec::new();

    let blur_enabled = state.render.blur_down_shader.is_some()
        && state.render.blur_up_shader.is_some()
        && state.render.blur_mask_shader.is_some();
    let mut blur_requests: Vec<BlurRequestData> = Vec::new();

    let focused_window = state.focus_root_window();

    #[cfg(feature = "profile-with-tracy")]
    let _windows_span = tracy_client::span!("compose::windows");
    #[cfg(feature = "profile-with-tracy")]
    let (mut visible_windows, mut shadow_elems) = (0u32, 0u32);
    for element in state.stage.windows().rev() {
        let window = match element {
            StageWindow::Client(w) => w,
            StageWindow::Suspended(s) => {
                // A fullscreen output shows only its fullscreen window.
                if output_fullscreen {
                    continue;
                }
                let Some(loc) = state.stage.position_of(element) else {
                    continue;
                };
                let bar = state.config.decorations.title_bar_height;
                let bw = state.default_border_width();
                let pad = driftwm::config::DecorationConfig::SHADOW_RADIUS.ceil() as i32 + bw;
                let size = s.size.get();
                let bbox = Rectangle::new(
                    Point::<i32, Logical>::from((loc.x - pad, loc.y - bar - pad)),
                    Size::<i32, Logical>::from((size.w + 2 * pad, size.h + bar + 2 * pad)),
                );
                let element_id = state.stage.id_of(element);
                if !visible_rect.overlaps(state.window_cull_rect(element_id, bbox)) {
                    continue;
                }
                // A stand-in's entry is position-only, so the whole slide lives
                // in `offset` and there is no stretch: its visual size always
                // equals the live size, both being `StageElement::size`.
                let animation = element_id.and_then(|id| {
                    let v = state.animated_visual(id, loc.to_f64(), size.to_f64());
                    (v.loc != loc.to_f64()).then(|| {
                        let physical_zoom = output_scale * zoom;
                        WindowRenderAnimation {
                            origin: Point::from((
                                (loc.x as f64 - camera.x) * physical_zoom,
                                (loc.y as f64 - camera.y) * physical_zoom,
                            )),
                            offset: Point::from((
                                (v.loc.x - loc.x as f64) * physical_zoom,
                                (v.loc.y - loc.y as f64) * physical_zoom,
                            )),
                            scale: Scale::from(1.0),
                        }
                    })
                });
                let focused = state.gated_suspended_focus() == Some(s.id);
                let launching = state.is_suspended_launching(s.id);
                let border_shader = state.render.border_shader.clone();
                let shadow_shader = state.render.shadow_shader.clone();
                suspended::push_suspended_element(
                    renderer,
                    s,
                    loc,
                    focused,
                    launching,
                    1.0,
                    animation,
                    &state.config.decorations,
                    state.decoration_scale,
                    &mut state.decorations,
                    &mut state.render.border_cache,
                    &mut state.render.shadow_cache,
                    border_shader.as_ref(),
                    shadow_shader.as_ref(),
                    camera,
                    zoom,
                    scale,
                    &mut zoomed_normal,
                );
                continue;
            }
        };
        // A window awaiting a deferred adopt is placed but not shown: the flush
        // teleports it into the stand-in's slot, and until then its rect is a
        // holding pattern the user would see it flash through. Before every push
        // below, not after — a partial push desyncs the element counts the blur
        // splice indexes with.
        if state.hidden_by_deferred_adopt(window) {
            continue;
        }
        let Some(loc) = state.stage.position_of(window) else {
            continue;
        };
        if output_fullscreen && !fullscreen_windows.contains(window) {
            continue;
        }
        let geom_loc = window.geometry().loc;
        let geom_size = window.geometry().size;
        let Some(wl_surface) = window.wl_surface() else {
            continue;
        };
        // Resolved once: `Stage::id_of` is a linear scan and three of the passes
        // below want the id.
        let element_id = state.stage.id_of(window);
        // Not stage membership: a resize freeze holds the pre-action picture on
        // screen after the stage has flipped, and a fullscreen leg trades the
        // chrome for the bare picture gradually instead of at one frame. Chrome
        // is built for as long as any of it is still visible.
        let chrome_alpha = state.chrome_alpha_of(element_id, window);
        let is_fullscreen = chrome_alpha <= 0.0;

        let applied = driftwm::config::applied_rule(&wl_surface);
        let is_widget = applied.as_ref().is_some_and(|r| r.widget);
        // Live pin membership — `window_render_transform` below decides the
        // canvas/screen frame from the same source, and the animation reference
        // frame has to agree with it.
        let is_pinned = state.is_pinned(window);
        // Whether the picture wears the pin: its title-bar marker, and normally
        // its z-bucket and blur layer too. Entering fullscreen unpins at the
        // action, and the freeze then holds the pinned picture on screen for the
        // rest of its budget — nothing may restack over a frame that isn't moving.
        let shows_pinned = state.pinned_picture_of(element_id, window);
        let bucket_pinned = state.draws_pinned_on(element_id, window, output_fullscreen);
        let is_focused = focused_window.as_ref() == Some(window);
        let effective_mode = driftwm::config::effective_decoration_mode(
            applied.as_ref().and_then(|r| r.decoration.as_ref()),
            &state.config.decorations.default_mode,
        );
        let configured = DrawnChrome {
            ssd_bar: state
                .decorations
                .contains_key(&DecorationKey::Surface(wl_surface.id())),
            border_width: driftwm::config::effective_border_width(
                applied.as_ref(),
                effective_mode,
                &state.config.decorations,
            ),
            corner_radius: driftwm::config::effective_corner_radius(
                applied.as_ref(),
                effective_mode,
                &state.config.decorations,
            ),
            shadow: driftwm::config::effective_shadow_enabled(
                applied.as_ref(),
                effective_mode,
                &state.config.decorations,
            ),
        };
        // The frame the window occupies — what the culling box below measures,
        // and what coverage is then judged on — carries bar and border whether or
        // not the border ends up drawn, so both can be read before there is a
        // render transform to decide coverage from.
        let frame_chrome = configured.drawn(is_fullscreen, canvas::Coverage::None);
        let has_ssd = frame_chrome.ssd_bar;
        let effective_bw = frame_chrome.border_width;
        let border_color = if is_focused {
            driftwm::config::effective_border_color_focused(
                applied.as_ref(),
                &state.config.decorations,
            )
        } else {
            driftwm::config::effective_border_color(applied.as_ref(), &state.config.decorations)
        };

        let mut bbox = window.bbox_with_popups();
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
        if !visible_rect.overlaps(state.window_cull_rect(element_id, bbox)) {
            continue;
        }

        // Captured before the per-window transform shadows `zoom`: the coverage
        // test below judges an entering window in the view the rest of the world
        // is still drawn through.
        let (world_camera, world_zoom) = (camera, zoom);
        // Centralized canvas↔screen decision: pinned windows render at their
        // output-relative `screen_pos` with zoom 1.0 (identity), normal windows
        // use the camera transform. `zoom` is shadowed so every downstream
        // scale (clip, border, shadow, blur, rescale) follows automatically.
        // A window growing into fullscreen is the one thing on the output that
        // belongs to the parked view rather than the world behind it.
        let (view_camera, view_zoom) = if entering_fullscreen.as_ref() == Some(window) {
            (live_camera, live_zoom)
        } else {
            (camera, zoom)
        };
        let Some((render_loc, zoom)) =
            state.window_render_transform(window, Some(output), view_camera, view_zoom)
        else {
            continue;
        };

        // Measured on the destination frame, never on the animated picture: a
        // window animation is eye candy over a state change that is already
        // complete, and input hit-tests the destination too. The camera half is
        // the opposite — the frame is mapped through the live view, so a fit's
        // camera flight keeps the chrome until the camera lands.
        // `render_loc + geom_loc` is the content origin fit, fill and parking
        // produced, or a pin's screen site, and committed geometry still reports
        // the pre-fit size until the client acks. Skipped outright when there is
        // nothing to suppress: the size read below takes a surface lock per
        // window per output per frame.
        let coverage =
            if (configured.corner_radius > 0 || configured.shadow || configured.border_width > 0)
                && !is_fullscreen
            {
                let bw = effective_bw as f64;
                let bar = if has_ssd {
                    state.config.decorations.title_bar_height as f64
                } else {
                    0.0
                };
                // A fullscreen entry parks the window on the output at the action,
                // long before the chrome has finished fading out, so through the
                // ramp coverage is judged on the rect it is growing out of — where
                // the picture is coming from — mapped through the view the world
                // behind it still renders through. An ordinary window fades its
                // corners and shadow as before; a fitted one was already square and
                // stays so.
                let leaving = (entering_fullscreen.as_ref() == Some(window))
                    .then(|| state.stage.fullscreen_on(&name))
                    .flatten();
                let (origin, content, scale) = match leaving {
                    Some(fs) => (
                        Point::from((
                            fs.saved_location.x as f64 - world_camera.x,
                            fs.saved_location.y as f64 - world_camera.y,
                        )),
                        fs.saved_size,
                        world_zoom,
                    ),
                    None => (
                        render_loc + geom_loc.to_f64(),
                        crate::state::configured_window_size(window),
                        zoom,
                    ),
                };
                let frame = Rectangle::new(
                    Point::from(((origin.x - bw) * scale, (origin.y - bar - bw) * scale)),
                    Size::from((
                        (content.w as f64 + 2.0 * bw) * scale,
                        (content.h as f64 + bar + 2.0 * bw) * scale,
                    )),
                );
                canvas::coverage(frame, usable)
            } else {
                canvas::Coverage::None
            };
        // Every chrome consumer below reads the decision, not the config: a
        // border hanging past the usable area is not drawn, even though the
        // frame measured above still counts it.
        let chrome = configured.drawn(is_fullscreen, coverage);
        let effective_bw = chrome.border_width;
        let effective_corner_radius = chrome.corner_radius;
        let effective_shadow = chrome.shadow;

        // Per-window lifecycle/geometry animation. A pinned window's chase runs
        // in screen space against its pin's content-box origin (`site.screen_pos`),
        // but `render_loc` is that origin minus `geom_loc` (the surface origin),
        // so add `geom_loc` back to align with what the Screen entry chases; a
        // normal window's reference is its canvas stage location.
        let anim_ref = if is_pinned {
            render_loc + geom_loc.to_f64()
        } else {
            loc.to_f64()
        };
        let target_size = geom_size.to_f64();
        let visual = element_id.map(|id| state.animated_visual(id, anim_ref, target_size));
        let (visual_alpha, window_animation) = match visual {
            Some(v) if v.loc != anim_ref || v.size != target_size || v.alpha != 1.0 => {
                let physical_zoom = output_scale * zoom;
                let content_origin = Point::from((
                    (render_loc.x + geom_loc.x as f64) * physical_zoom,
                    (render_loc.y + geom_loc.y as f64) * physical_zoom,
                ));
                let animation = WindowRenderAnimation {
                    origin: content_origin,
                    offset: Point::from((
                        (v.loc.x - anim_ref.x) * physical_zoom,
                        (v.loc.y - anim_ref.y) * physical_zoom,
                    )),
                    // Never magnifies a buffer the client has not redrawn yet.
                    scale: Scale::from(crate::state::window_animation::content_scale(
                        v.size,
                        target_size,
                        v.cap_content,
                    )),
                };
                (v.alpha, Some(animation))
            }
            _ => (1.0, None),
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
        // A frozen exit still wears the fullscreen picture over a culled canvas,
        // and an entry ramp is already fullscreen on the stage — neither may dim.
        let fullscreen_exempt = is_fullscreen || state.stage.is_fullscreen(window);
        let window_blur = driftwm::config::effective_blur(
            applied.as_ref(),
            &state.config.decorations,
            fullscreen_exempt,
        );
        let wants_blur = blur_enabled && (window_blur || client_blur);
        let opacity = driftwm::config::effective_opacity(
            applied.as_ref(),
            &state.config.decorations,
            is_focused,
            fullscreen_exempt,
        ) * visual_alpha as f64;
        // Bar, border and shadow ride the fullscreen ramp on top of the window's
        // own opacity; `chrome_alpha` is 1 for every other window.
        let chrome_opacity = opacity * chrome_alpha as f64;

        // A resize crossfade rides the window's own transform, so the old content
        // lands on the interpolated visual rect for Canvas and Screen entries
        // alike. Positioned at the *live* geometry rect, which that transform
        // then maps exactly as it maps the live content.
        let resize_overlay = element_id
            .and_then(|id| state.resize_crossfades.get(&id))
            .map(|crossfade| {
                let geometry_phys: Point<f64, Physical> = Point::from((
                    (render_loc.x + geom_loc.x as f64) * zoom * output_scale,
                    (render_loc.y + geom_loc.y as f64) * zoom * output_scale,
                ));
                let geometry_size: Size<i32, Logical> = Size::from((
                    (geom_size.w as f64 * zoom).round() as i32,
                    (geom_size.h as f64 * zoom).round() as i32,
                ));
                crossfade.render_element(geometry_phys, geometry_size, window_animation, opacity)
            });

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

        // Test pinned BEFORE `is_widget`: a pinned *widget* must land in the
        // pinned bucket (above normal), not `zoomed_widgets` (below normal).
        let target = if bucket_pinned {
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
        push_plain_elements(target, popup_elems, zoom, window_animation);
        // Where a resize crossfade goes: above the live content, below the SSD
        // bar and popups. Spliced in at the end so the branches below stay one
        // content push each.
        let mut overlay_at = target.len();

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
            if let Some(deco) = state
                .decorations
                .get_mut(&DecorationKey::Surface(wl_surface.id()))
            {
                deco.update(
                    geom_size.w,
                    is_focused,
                    shows_pinned,
                    state.decoration_scale,
                    &deco_title,
                    effective_corner_radius,
                    &state.config.decorations,
                );
            }

            if let Some(deco) = state
                .decorations
                .get(&DecorationKey::Surface(wl_surface.id()))
            {
                let bar_physical: Point<f64, Physical> =
                    Point::from((loc_phys.x as f64, loc_phys.y as f64 - bar_h_phys));
                let bar_alpha = if chrome_opacity < 1.0 {
                    Some(chrome_opacity as f32)
                } else {
                    None
                };
                // The bar takes the border's inner width so it stops where the
                // content does instead of covering the ring's inner stroke.
                let bar_dst = Rectangle::new(
                    Point::from((loc_phys.x, loc_phys.y - bar_h_phys as i32)),
                    painted_size(
                        Size::<f64, Logical>::from((geom_size.w as f64, bar_h_logical)),
                        scale,
                    ),
                );
                if let Ok(bar_elem) = MemoryRenderBufferRenderElement::from_buffer(
                    renderer,
                    bar_physical,
                    &deco.title_bar,
                    bar_alpha,
                    None,
                    None,
                    Kind::Unspecified,
                ) {
                    let bar_elem = PixelSnapRescaleElement::from_element(
                        TrimmedElement::from_element(bar_elem, bar_dst),
                        Point::<i32, Physical>::from((0, 0)),
                        zoom,
                    );
                    if let Some(animation) = window_animation {
                        target.push(OutputRenderElements::AnimatedTrimmedDecoration(
                            WindowTransformElement::new(
                                bar_elem,
                                animation.origin,
                                animation.offset,
                                animation.scale,
                            ),
                        ));
                    } else {
                        target.push(OutputRenderElements::TrimmedDecoration(bar_elem));
                    }
                }
            }
            overlay_at = target.len();

            // Only bottom corners round (title bar covers the top edge).
            if let Some(ref shader) = state.render.corner_clip_shader {
                let radius = effective_corner_radius as f32;
                if effective_bw > 0 || radius > 0.0 {
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
                        window_animation,
                    );
                } else {
                    push_plain_elements(target, elems, zoom, window_animation);
                }
            } else {
                push_plain_elements(target, elems, zoom, window_animation);
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
                    wl_surface.id().into(),
                    &shader,
                    inner_logical,
                    effective_corner_radius as f32,
                    effective_bw,
                    border_color,
                    is_focused,
                    chrome_opacity,
                    scale,
                    zoom,
                    window_animation,
                );
            }

            // Shadow encloses title bar + content + border; cached per-surface
            // so the damage tracker can skip unchanged regions. With a border,
            // the footprint grows by border_width so the shadow grades from the
            // border's outer perimeter; the radius follows `outer_corner_radius`,
            // so a square window keeps a square shadow.
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
                    wl_surface.id().into(),
                    &shader,
                    body_logical,
                    outer_corner_radius(effective_corner_radius as f32, effective_bw as f32),
                    chrome_opacity,
                    scale,
                    zoom,
                    window_animation,
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
                    window_animation,
                );

                if effective_bw > 0
                    && let Some(border_shader) = state.render.border_shader.clone()
                {
                    push_border_element(
                        target,
                        &mut state.render.border_cache,
                        wl_surface.id().into(),
                        &border_shader,
                        geometry,
                        radius,
                        effective_bw,
                        border_color,
                        is_focused,
                        chrome_opacity,
                        scale,
                        zoom,
                        window_animation,
                    );
                }

                // The footprint grows by border_width so the shadow grades from
                // the border's outer edge, not the content edge; the radius
                // follows `outer_corner_radius`, so a square window keeps a
                // square shadow.
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
                        wl_surface.id().into(),
                        &shader,
                        body_logical,
                        outer_corner_radius(effective_corner_radius as f32, effective_bw as f32),
                        chrome_opacity,
                        scale,
                        zoom,
                        window_animation,
                    );
                    shadow_count = 1;
                }
            } else {
                // Bare (`decoration = "none"`) or fullscreen: pass through.
                push_plain_elements(target, elems, zoom, window_animation);
            }
        } else {
            push_plain_elements(target, elems, zoom, window_animation);
        }

        // In-bucket, ahead of the trailing shadow, so the blur element counts
        // below stay valid.
        if let Some(overlay) = resize_overlay {
            target.insert(overlay_at, overlay);
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
            let screen_rect = painted_rect(
                Rectangle::new(
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
                .to_f64(),
                Scale::from(output_scale),
            );
            // Frost tracks the animated window's visual rect, not its instant
            // logical position (accepted cost: a frosted animation re-blurs at
            // frame rate, throttled by `animate_blur_fps` like a drag).
            let screen_rect =
                window_animation.map_or(screen_rect, |anim| anim.transform_phys_rect(screen_rect));

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

            // If all client rects clipped to nothing AND nothing else asked
            // for blur, skip — otherwise region_rects=None would be interpreted
            // as whole-window blur, against what the client requested.
            let skip_clipped_out = client_blur && region_rects.is_none() && !window_blur;

            if !skip_clipped_out {
                blur_requests.push(BlurRequestData {
                    surface_id: wl_surface.id(),
                    screen_rect,
                    elem_start,
                    elem_count,
                    // Measured, not derived from the config: the shadow only
                    // pushes when its shader compiled.
                    trailing_chrome: shadow_count,
                    // Must follow the bucket the elements went into, since the
                    // blur splices by that bucket's prefix offset. The tag also
                    // marks a layer as screen-fixed, so its blur recomputes on
                    // camera moves — moot for a picture giving the bucket up,
                    // since a fullscreen output's camera is locked and there is
                    // no scene left behind it to pan anyway.
                    layer: if bucket_pinned {
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

    // Closing snapshots + adoption fades draw above normal windows, but never on
    // a visually-fullscreen output (no dying-window flash over a fullscreen app).
    if !output_fullscreen {
        zoomed_closing = closing::render_snapshots_for_output(
            &state.closing_snapshots,
            &name,
            visible_rect,
            camera,
            zoom,
            output_scale,
        );
        // Collected before the per-fade mutable borrows of `state` below.
        // `focused`/`launching` are the values frozen at fade creation, never a
        // live lookup: adoption already ended the relaunch and moved focus, and
        // the chrome caches key on both, so re-resolving them would re-rasterize
        // the label and re-color the bar mid-fade.
        let fades: Vec<FadeRender> = state
            .standin_fades
            .iter()
            .filter(|f| visible_rect.overlaps(Rectangle::new(f.loc, f.suspended.size.get())))
            .map(|f| {
                let shrink = f.shrink_scale();
                let size = f.suspended.size.get();
                let bar = state.config.decorations.title_bar_height;
                // Shrink toward the centre of the frame the stand-in occupied
                // (its body plus the bar strip above it).
                let centre = Point::<f64, Physical>::from((
                    (f.loc.x as f64 - camera.x + size.w as f64 / 2.0) * zoom * output_scale,
                    (f.loc.y as f64 - camera.y - bar as f64 / 2.0 + size.h as f64 / 2.0)
                        * zoom
                        * output_scale,
                ));
                FadeRender {
                    suspended: f.suspended.clone(),
                    loc: f.loc,
                    focused: f.focused,
                    launching: f.launching,
                    alpha: f.alpha(),
                    animation: (shrink < 1.0).then(|| WindowRenderAnimation {
                        origin: centre,
                        offset: Point::default(),
                        scale: Scale::from(shrink),
                    }),
                }
            })
            .collect();
        for fade in fades {
            let border_shader = state.render.border_shader.clone();
            let shadow_shader = state.render.shadow_shader.clone();
            suspended::push_suspended_element(
                renderer,
                &fade.suspended,
                fade.loc,
                fade.focused,
                fade.launching,
                fade.alpha,
                fade.animation,
                &state.config.decorations,
                state.decoration_scale,
                &mut state.decorations,
                &mut state.render.border_cache,
                &mut state.render.shadow_cache,
                border_shader.as_ref(),
                shadow_shader.as_ref(),
                camera,
                zoom,
                scale,
                &mut zoomed_closing,
            );
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

    // All three sit below the windows, so a fullscreen window occludes them
    // unless it is translucent enough to see the canvas through. A widget riding
    // this bucket comes back with it while one written as a rule-placed toplevel
    // stays culled with every other window — two identical-looking widgets split
    // by protocol. Accepted: drawing toplevels through a fullscreen window is a
    // separate feature.
    let canvas_layer_elements = if fullscreen_conceals {
        Vec::new()
    } else {
        build_canvas_layer_elements(state, renderer, output_scale, camera, zoom, visible_rect)
    };

    let outline_elements = if fullscreen_conceals {
        Vec::new()
    } else {
        build_output_outline_elements(state, renderer, output, camera, zoom, viewport_size)
    };

    let bg_elements: Vec<OutputRenderElements> = if fullscreen_conceals {
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

    #[cfg(feature = "profile-with-tracy")]
    let _layers_span = tracy_client::span!("compose::layers");
    let (overlay_elements, overlay_blur) = build_layer_elements(
        state,
        output,
        renderer,
        WlrLayer::Overlay,
        Some(BlurLayer::Overlay),
    );
    let (top_elements, top_blur) = if !output_fullscreen {
        build_layer_elements(state, output, renderer, WlrLayer::Top, Some(BlurLayer::Top))
    } else {
        (vec![], vec![])
    };
    let (bottom_elements, _) = if !output_fullscreen {
        build_layer_elements(state, output, renderer, WlrLayer::Bottom, None)
    } else {
        (vec![], vec![])
    };
    let (background_layer_elements, _) = if !output_fullscreen {
        build_layer_elements(state, output, renderer, WlrLayer::Background, None)
    } else {
        (vec![], vec![])
    };
    #[cfg(feature = "profile-with-tracy")]
    drop(_layers_span);

    // Prefix offsets locate each group in all_elements for blur insertion. The
    // closing bucket sits between pinned and normal, so `normal_prefix` counts
    // it and the normal windows' blur indices stay correct.
    let overlay_prefix = cursor_elements.len();
    let top_prefix = overlay_prefix + overlay_elements.len();
    let pinned_prefix = top_prefix + top_elements.len();
    let normal_prefix = pinned_prefix + zoomed_pinned.len() + zoomed_closing.len();
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
            + zoomed_closing.len()
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
    all_elements.extend(zoomed_closing);
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
            camera,
            zoom,
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
        // Prune only this output's stale entries: another output's caches are
        // keyed under its own name and must survive this output's frame.
        let name = output.name();
        state
            .render
            .blur_cache
            .retain(|(out, id), _| out != &name || active_ids.contains(id));
        if active_ids.is_empty() {
            // The shared backdrop's two output-sized textures and the scratch
            // pool, with nothing left to sample or pace them.
            // `process_blur_requests` — the only other place that frees them —
            // never runs on a frame without requests, so they would strand here
            // until the output went away.
            state.render.remove_blur_caches(&name);
        }
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

/// Identifies an outline strip buffer by everything that decides its *content*:
/// the configured colour and the clipped strip extent. Position is deliberately
/// absent — it belongs to the render element, and keying on it would mint a
/// fresh buffer (hence a fresh element `Id`) on every frame of a camera pan.
pub type OutlineBufferKey = ([u8; 4], i32, i32);

/// The visible parts of an outline's four edges, clipped to the viewport.
/// Edges that fall entirely outside it are dropped.
fn outline_edge_strips(
    outline: Rectangle<i32, Logical>,
    viewport_size: Size<i32, Logical>,
    thickness: i32,
) -> impl Iterator<Item = Rectangle<i32, Logical>> {
    let (loc, size) = (outline.loc, outline.size);
    let edges = [
        Rectangle::new(loc, Size::from((size.w, thickness))),
        Rectangle::new(
            Point::from((loc.x, loc.y + size.h - thickness)),
            Size::from((size.w, thickness)),
        ),
        Rectangle::new(loc, Size::from((thickness, size.h))),
        Rectangle::new(
            Point::from((loc.x + size.w - thickness, loc.y)),
            Size::from((thickness, size.h)),
        ),
    ];
    let viewport = Rectangle::from_size(viewport_size);
    edges
        .into_iter()
        .filter_map(move |e| e.intersection(viewport))
}

/// The solid-colour buffer for one outline strip, reused across frames.
///
/// `MemoryRenderBuffer::from_slice` mints a fresh `Id`, which the render element
/// inherits — rebuilding per frame therefore re-damages every outline every
/// frame and makes the blur's background hash differ every frame.
fn outline_buffer(
    cache: &mut HashMap<OutlineBufferKey, MemoryRenderBuffer>,
    color: [u8; 4],
    strip: Rectangle<i32, Logical>,
) -> &MemoryRenderBuffer {
    let (w, h) = (strip.size.w, strip.size.h);
    cache.entry((color, w, h)).or_insert_with(|| {
        let pixels: Vec<u8> = vec![color[0], color[1], color[2], color[3]]
            .into_iter()
            .cycle()
            .take((w * h) as usize * 4)
            .collect();

        MemoryRenderBuffer::from_slice(
            &pixels,
            Fourcc::Abgr8888,
            (w, h),
            1,
            Transform::Normal,
            None,
        )
    })
}

/// Thin outlines showing where other monitors' viewports sit on the canvas.
fn build_output_outline_elements(
    state: &mut crate::state::DriftWm,
    renderer: &mut GlesRenderer,
    output: &Output,
    camera: Point<f64, Logical>,
    zoom: f64,
    viewport_size: Size<i32, Logical>,
) -> Vec<OutputRenderElements> {
    let thickness = state.config.output_outline.thickness;
    let opacity = state.config.output_outline.opacity as f32;
    if thickness <= 0 || opacity <= 0.0 {
        state.render.cached_outlines.remove(&output.name());
        return vec![];
    }

    let color = state.config.output_outline.color;
    let scale = output.current_scale().fractional_scale();

    let mut strips: Vec<Rectangle<i32, Logical>> = Vec::new();

    for other in state.space.outputs() {
        if *other == *output {
            continue;
        }
        // A fullscreen output shows a screen-space window, not a canvas
        // viewport, so it has no outline to project onto other monitors (once
        // the fullscreen-entry transition has covered the canvas). Coverage, not
        // concealment: a translucent fullscreen output draws its neighbours'
        // outlines but still projects none of its own, because a canvas showing
        // through a window is not a viewport worth pointing at.
        if state.is_output_visually_fullscreen(other) {
            continue;
        }

        // The view that output is showing its canvas at, which through a
        // fullscreen entry is still the pre-park one.
        let (other_camera, other_zoom) = state.world_view(other);
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

        strips.extend(outline_edge_strips(outline_rect, viewport_size, thickness));
    }

    let cache = state
        .render
        .cached_outlines
        .entry(output.name())
        .or_default();
    let mut elements = Vec::with_capacity(strips.len());
    let mut live: Vec<OutlineBufferKey> = Vec::with_capacity(strips.len());

    for strip in strips {
        live.push((color, strip.size.w, strip.size.h));
        let buf = outline_buffer(cache, color, strip);

        let loc: Point<f64, Physical> = strip.loc.to_f64().to_physical(scale);
        if let Ok(elem) = MemoryRenderBufferRenderElement::from_buffer(
            renderer,
            loc,
            buf,
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

    // A strip clipped by the viewport changes extent as the camera pans, so
    // without a prune the cache would grow one entry per pixel of travel.
    cache.retain(|key, _| live.contains(key));

    elements
}

#[cfg(test)]
mod texel_src_tests {
    use super::*;

    /// The whole point of [`texel_src`]: for a scale-1-wrapped offscreen the src
    /// must span every texel, which on a 2x surface is twice the destination
    /// extent per axis. Passing the destination (what `src: None` falls back to)
    /// would sample a quarter of the texture and magnify it.
    #[test]
    fn texel_src_spans_the_whole_texture_not_the_destination() {
        let logical: Size<i32, Logical> = Size::from((800, 600));
        let scale = 2.0;
        let texels: Size<i32, Physical> = Size::from((
            (logical.w as f64 * scale) as i32,
            (logical.h as f64 * scale) as i32,
        ));

        let src = texel_src(texels);
        assert_eq!(src.loc, Point::from((0.0, 0.0)));
        assert_eq!(src.size.w, texels.w as f64, "src covers every texel across");
        assert_eq!(src.size.h, texels.h as f64, "src covers every texel down");
        assert_ne!(
            src.size.w, logical.w as f64,
            "src must not collapse to the destination extent — that is the bug"
        );
    }

    /// Fractional scales round-trip too (the reason src is carried in texels
    /// rather than derived from an integer buffer scale).
    #[test]
    fn texel_src_handles_a_fractional_scale() {
        let texels: Size<i32, Physical> = Size::from((1200, 900));
        let src = texel_src(texels);
        assert_eq!(src.size.w, 1200.0);
        assert_eq!(src.size.h, 900.0);
    }
}

#[cfg(test)]
mod outline_cache_tests {
    use super::*;

    const WHITE: [u8; 4] = [0xFF, 0xFF, 0xFF, 0xFF];

    fn strips(
        outline: Rectangle<i32, Logical>,
        viewport: Size<i32, Logical>,
    ) -> Vec<Rectangle<i32, Logical>> {
        outline_edge_strips(outline, viewport, 1).collect()
    }

    #[test]
    fn a_fully_visible_outline_yields_four_edges() {
        let viewport = Size::from((1920, 1080));
        let outline = Rectangle::new(Point::from((100, 200)), Size::from((400, 300)));

        let edges = strips(outline, viewport);
        assert_eq!(edges.len(), 4);
        assert_eq!(
            edges[0],
            Rectangle::new(Point::from((100, 200)), Size::from((400, 1)))
        );
        assert_eq!(
            edges[1],
            Rectangle::new(Point::from((100, 499)), Size::from((400, 1)))
        );
        assert_eq!(
            edges[2],
            Rectangle::new(Point::from((100, 200)), Size::from((1, 300)))
        );
        assert_eq!(
            edges[3],
            Rectangle::new(Point::from((499, 200)), Size::from((1, 300)))
        );
    }

    #[test]
    fn edges_are_clipped_to_the_viewport_and_offscreen_ones_dropped() {
        let viewport = Size::from((1920, 1080));
        // Hangs off the left edge: the left edge is gone entirely, the
        // horizontal ones survive as their visible remainder.
        let outline = Rectangle::new(Point::from((-50, 200)), Size::from((400, 300)));

        let edges = strips(outline, viewport);
        assert_eq!(edges.len(), 3);
        assert_eq!(
            edges[0],
            Rectangle::new(Point::from((0, 200)), Size::from((350, 1)))
        );
        assert_eq!(
            edges[1],
            Rectangle::new(Point::from((0, 499)), Size::from((350, 1)))
        );
        assert_eq!(
            edges[2],
            Rectangle::new(Point::from((349, 200)), Size::from((1, 300)))
        );
    }

    /// The reason the cache key excludes position: panning moves every strip
    /// but changes no strip's extent, so the buffers — and with them the
    /// element `Id`s the blur hashes — must survive the pan.
    #[test]
    fn panning_a_visible_outline_reuses_every_buffer() {
        let viewport = Size::from((1920, 1080));
        let mut cache = HashMap::new();

        for offset in 0..20 {
            let outline = Rectangle::new(
                Point::from((100 + offset, 200 + offset)),
                Size::from((400, 300)),
            );
            for strip in strips(outline, viewport) {
                outline_buffer(&mut cache, WHITE, strip);
            }
        }

        assert_eq!(
            cache.len(),
            2,
            "one buffer per distinct extent: the horizontal pair and the vertical pair"
        );
    }

    #[test]
    fn a_different_extent_or_colour_gets_its_own_buffer() {
        let mut cache = HashMap::new();
        let strip = Rectangle::new(Point::from((0, 0)), Size::from((400, 1)));

        outline_buffer(&mut cache, WHITE, strip);
        outline_buffer(
            &mut cache,
            WHITE,
            Rectangle::new(Point::from((0, 0)), Size::from((399, 1))),
        );
        assert_eq!(cache.len(), 2, "a clipped strip is a different buffer");

        outline_buffer(&mut cache, [0, 0, 0, 0xFF], strip);
        assert_eq!(
            cache.len(),
            3,
            "colour is baked into the pixels, so it keys"
        );
    }
}
