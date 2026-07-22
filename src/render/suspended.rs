//! Rendering for suspended windows: a solid body in the SSD title-bar
//! background color, the app name centered in it, and the reused SSD title bar,
//! border, and shadow. Chrome caches (title bar, border, shadow) key on the
//! suspended id; the body/label buffers live on the element, rebuilt only when
//! the size, scale, or "launching…" flag change.

use std::collections::HashMap;
use std::rc::Rc;

use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::element::memory::{
    MemoryRenderBuffer, MemoryRenderBufferRenderElement,
};
use smithay::backend::renderer::gles::{GlesPixelProgram, GlesRenderer};
use smithay::utils::{Logical, Physical, Point, Rectangle, Scale, Size, Transform};

use driftwm::config::DecorationConfig;

use crate::decorations::{DecorationKey, WindowDecoration};
use crate::render::PixelSnapRescaleElement;
use crate::render::elements::OutputRenderElements;
use crate::render::shaders::{push_border_element, push_shadow_element};
use crate::state::{BorderCacheEntry, ShadowCacheEntry, SuspendedWindow};

/// Points → logical pixels (96 dpi), matching the title bar's font sizing.
const PT_TO_PX: f32 = 4.0 / 3.0;
/// Horizontal padding inside the body before the label ellipsizes.
const LABEL_SIDE_PAD: i32 = 24;

/// Rebuild the label buffer (and record its body-local rect) for the current
/// body size / scale / launching / font-readiness state. Writes into the
/// element's `chrome` cell. `fonts_ready` is part of the key so a label built
/// before the background font scan completes re-rasters with text once it
/// lands, rather than keeping its empty launch-time text forever.
pub(crate) fn ensure_label(
    s: &SuspendedWindow,
    body: Size<i32, Logical>,
    scale: i32,
    launching: bool,
    fonts_ready: bool,
    config: &DecorationConfig,
) {
    let key = (body.w, body.h, scale, launching, fonts_ready);
    if s.chrome.borrow().label_key == Some(key) {
        return;
    }

    let text = if launching {
        format!("{} launching…", s.identity.display_name)
    } else {
        s.identity.display_name.clone()
    };
    let font_px = (config.font_size as f32 * PT_TO_PX * scale as f32).max(1.0);
    let avail = (body.w - 2 * LABEL_SIDE_PAD).max(1) * scale;
    let (fitted, text_w) =
        driftwm::text::fit_text(&text, &config.font, font_px, config.font_weight, avail);

    let mut chrome = s.chrome.borrow_mut();
    if fitted.is_empty() || text_w <= 0 {
        chrome.label = None;
        chrome.label_rect = None;
        chrome.label_key = Some(key);
        return;
    }

    let buf_w = text_w;
    let buf_h = (font_px * 1.5).ceil() as i32;
    let mut pixels = vec![0u8; (buf_w * buf_h * 4) as usize];
    driftwm::text::rasterize_into(
        &mut pixels,
        buf_w,
        buf_h,
        &fitted,
        &config.font,
        font_px,
        config.font_weight,
        config.fg_color,
        0,
    );
    chrome.label = Some(MemoryRenderBuffer::from_slice(
        &pixels,
        Fourcc::Abgr8888,
        (buf_w, buf_h),
        scale,
        Transform::Normal,
        None,
    ));
    // Body-local rect for the Label hit region, centered in the body.
    let lw = (buf_w / scale).max(1);
    let lh = (buf_h / scale).max(1);
    chrome.label_rect = Some(Rectangle::new(
        Point::from(((body.w - lw) / 2, (body.h - lh) / 2)),
        Size::from((lw, lh)),
    ));
    chrome.label_key = Some(key);
}

/// Rebuild the rounded body buffer for the current size / scale. The bar always
/// covers the top corners, so only the bottom pair rounds. Writes into the
/// element's `chrome` cell.
pub(crate) fn ensure_body(
    s: &SuspendedWindow,
    body: Size<i32, Logical>,
    scale: i32,
    config: &DecorationConfig,
) {
    let key = (body.w, body.h, scale);
    if s.chrome.borrow().body_key == Some(key) {
        return;
    }
    let buf = crate::decorations::render_body_fill(body.w, body.h, scale, config);
    let mut chrome = s.chrome.borrow_mut();
    chrome.body = Some(buf);
    chrome.body_key = Some(key);
}

/// Emit the render elements for a suspended window into `target` (the
/// non-widget canvas bucket). Takes the chrome caches as disjoint borrows so
/// the caller can keep the stage iterator alive.
#[allow(clippy::too_many_arguments)]
pub(super) fn push_suspended_element(
    renderer: &mut GlesRenderer,
    s: &Rc<SuspendedWindow>,
    loc: Point<i32, Logical>,
    focused: bool,
    launching: bool,
    config: &DecorationConfig,
    decoration_scale: i32,
    decorations: &mut HashMap<DecorationKey, WindowDecoration>,
    border_cache: &mut HashMap<DecorationKey, BorderCacheEntry>,
    shadow_cache: &mut HashMap<DecorationKey, ShadowCacheEntry>,
    border_shader: Option<&GlesPixelProgram>,
    shadow_shader: Option<&GlesPixelProgram>,
    camera: Point<f64, Logical>,
    zoom: f64,
    scale: Scale<f64>,
    target: &mut Vec<OutputRenderElements>,
) {
    let key = DecorationKey::Suspended(s.id);
    let size = s.size.get();
    // Every stand-in draws the same bar; a CSD-origin one already shrank its
    // body under it, so the outer footprint matches the window it replaced.
    let bar_height = config.title_bar_height;

    // Pre-zoom, output-relative logical origin (geometry loc is 0 for a
    // suspended window, and they're never pinned/fullscreen).
    let render_loc: Point<f64, Logical> =
        Point::from((loc.x as f64 - camera.x, loc.y as f64 - camera.y));
    let loc_phys: Point<i32, Physical> = render_loc.to_physical_precise_round(scale);

    let bar_h_phys = (bar_height as f64 * scale.y).round();
    let bar_h_logical = bar_h_phys / scale.y;

    // Centered app-name label. Pushed BEFORE the body fill: earlier in the
    // vec is topmost in smithay z-order, so the opaque body must sit below the
    // label or it occludes it.
    ensure_label(
        s,
        size,
        decoration_scale,
        launching,
        driftwm::text::fonts_ready(),
        config,
    );
    {
        let chrome = s.chrome.borrow();
        if let (Some(buf), Some(rect)) = (chrome.label.as_ref(), chrome.label_rect) {
            let label_x = render_loc.x + rect.loc.x as f64;
            let label_y = render_loc.y + rect.loc.y as f64;
            let label_phys: Point<f64, Physical> =
                Point::from((label_x * scale.x, label_y * scale.y));
            if let Ok(elem) = MemoryRenderBufferRenderElement::from_buffer(
                renderer,
                label_phys,
                buf,
                None,
                None,
                None,
                Kind::Unspecified,
            ) {
                target.push(OutputRenderElements::Decoration(
                    PixelSnapRescaleElement::from_element(
                        elem,
                        Point::<i32, Physical>::from((0, 0)),
                        zoom,
                    ),
                ));
            }
        }
    }

    // Rounded body fill in the SSD background color, below the label. The bar
    // covers the top corners, so only the bottom pair rounds.
    ensure_body(s, size, decoration_scale, config);
    {
        let chrome = s.chrome.borrow();
        if let Some(buf) = chrome.body.as_ref() {
            let body_phys: Point<f64, Physical> =
                Point::from((loc_phys.x as f64, loc_phys.y as f64));
            if let Ok(body_elem) = MemoryRenderBufferRenderElement::from_buffer(
                renderer,
                body_phys,
                buf,
                None,
                None,
                None,
                Kind::Unspecified,
            ) {
                target.push(OutputRenderElements::Decoration(
                    PixelSnapRescaleElement::from_element(
                        body_elem,
                        Point::<i32, Physical>::from((0, 0)),
                        zoom,
                    ),
                ));
            }
        }
    }

    // Title bar via the reused windowless rasterizer. Rendered textless (the
    // centered label already names the app); the close button and its hover
    // highlight stay.
    let deco = decorations
        .entry(key.clone())
        .or_insert_with(|| WindowDecoration::new(size.w, focused, config));
    deco.update(size.w, focused, false, decoration_scale, "", config);
    let bar_physical: Point<f64, Physical> =
        Point::from((loc_phys.x as f64, loc_phys.y as f64 - bar_h_phys));
    if let Ok(bar_elem) = MemoryRenderBufferRenderElement::from_buffer(
        renderer,
        bar_physical,
        &deco.title_bar,
        None,
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

    // Border + shadow around title bar + body, keyed by the suspended id.
    let mode = driftwm::config::effective_decoration_mode(None, &config.default_mode);
    let border_width = driftwm::config::effective_border_width(None, mode, config);
    let corner_radius = driftwm::config::effective_corner_radius(None, mode, config);
    let border_color = if focused {
        driftwm::config::effective_border_color_focused(None, config)
    } else {
        driftwm::config::effective_border_color(None, config)
    };

    if border_width > 0
        && let Some(shader) = border_shader
    {
        let inner_logical: Rectangle<f64, Logical> = Rectangle::new(
            (render_loc.x, render_loc.y - bar_h_logical).into(),
            (size.w as f64, size.h as f64 + bar_h_logical).into(),
        );
        push_border_element(
            target,
            border_cache,
            key.clone(),
            shader,
            inner_logical,
            corner_radius as f32,
            border_width,
            border_color,
            focused,
            1.0,
            scale,
            zoom,
        );
    }

    if driftwm::config::effective_shadow_enabled(None, mode, config)
        && let Some(shader) = shadow_shader
    {
        let bw = border_width as f64;
        let body_logical: Rectangle<f64, Logical> = Rectangle::new(
            (render_loc.x - bw, render_loc.y - bar_h_logical - bw).into(),
            (
                size.w as f64 + 2.0 * bw,
                size.h as f64 + bar_h_logical + 2.0 * bw,
            )
                .into(),
        );
        push_shadow_element(
            target,
            shadow_cache,
            key,
            shader,
            body_logical,
            (corner_radius + border_width) as f32,
            1.0,
            scale,
            zoom,
        );
    }
}
