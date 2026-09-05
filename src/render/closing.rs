use std::collections::HashMap;
use std::rc::Rc;
use std::time::{Duration, Instant};

use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::element::memory::{
    MemoryRenderBuffer, MemoryRenderBufferRenderElement,
};
use smithay::backend::renderer::element::texture::{TextureBuffer, TextureRenderElement};
use smithay::backend::renderer::element::{Element as _, Kind, RenderElement};
use smithay::backend::renderer::gles::{
    GlesPixelProgram, GlesRenderer, GlesTexProgram, GlesTexture, Uniform, UniformValue,
};
use smithay::backend::renderer::utils::{RendererSurfaceStateUserData, import_surface};
use smithay::backend::renderer::{Bind as _, Color32F, Frame as _, Offscreen, Renderer as _};
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::user_data::UserDataMap;
use smithay::utils::{Logical, Physical, Point, Rectangle, Scale, Size, Transform};
use smithay::wayland::compositor::{TraversalAction, with_surface_tree_downward};

use driftwm::stage::ElementId;

use crate::state::SuspendedWindow;

use super::{
    OutputRenderElements, TrimmedElement, WindowRenderAnimation, WindowTransformElement,
    painted_rect, shaders,
};

/// A progress-based effect ends when within this of 1.0 (a 1% alpha residue is
/// invisible; a tighter epsilon leaves a long, dead tail past the motion).
const DONE_EPSILON: f64 = 0.01;

/// Alpha of a closing or departing effect at `progress`: `1 - p²`. Shared by the
/// close fade, both suspend crossfades, and the shrinking half of a resize.
///
/// Eased rather than linear so the window holds near-opaque while most of the
/// shrink plays (0.91 at p=0.3) instead of dwelling translucent, which reads as a
/// far stronger fade than intended. A smooth quadratic rather than a clamped ramp
/// leaves no saturation corner, and it reaches 0 exactly at p=1.
fn fade_out_alpha(progress: f64) -> f32 {
    let p = progress.clamp(0.0, 1.0);
    (1.0 - p * p) as f32
}

/// Alpha of the outgoing half of a resize crossfade at `progress`, picked by the
/// leg's direction: front-loaded `(1 - p)²` for a grow, [`fade_out_alpha`]'s
/// back-loaded `1 - p²` for a shrink.
///
/// This is a morph, not a departure: both pictures are drawn into the *same*
/// interpolated rect, so at every frame the old is scaled by `rect / old` and
/// the new by `rect / new`. Whichever native is smaller is therefore the more
/// magnified — and so the softer — of the two at every frame, whatever the rect
/// is doing and whatever the zoom. The fade belongs on that one.
///
/// A grow makes the old picture the softer one, so it goes early: `(1 - p)²`
/// leaves a quarter of it at the halfway point, against half under a linear
/// ramp. A shrink makes the new picture the softer one, so the old is held and
/// handed over late instead.
///
/// Handing over is safe at any point because the new buffer is already committed
/// underneath: the outgoing picture is strictly an overlay, so the fade reveals
/// real content and can never expose a gap.
fn crossfade_out_alpha(progress: f64, grew: bool) -> f32 {
    if !grew {
        return fade_out_alpha(progress);
    }
    let remaining = 1.0 - progress.clamp(0.0, 1.0);
    (remaining * remaining) as f32
}

/// Whether a resize leg from `old` to `new` is a grow: whether the leg's target
/// has more area than the captured picture. Area rather than per-axis because a
/// leg can grow one axis while shrinking the other; a tie is harmless either
/// way, it just has to pick one.
///
/// Deliberately compares the two *natives* rather than the leg's own rects. A
/// rect-based criterion looks equivalent and is not: fullscreen entry seeds its
/// `from` rect as the windowed size times the saved zoom, against a
/// viewport-sized target, so at zoom 4 an 800x600 window going fullscreen on a
/// 1920x1080 output plays as a 3200x2400 → 1920x1080 *shrinking* rect. Reading
/// that as a shrink would back-load the fade and hold a 4x-magnified old picture
/// through the whole grow. The natives are what the two textures actually are,
/// so they answer the same at any zoom.
fn leg_grew(old: Size<i32, Logical>, new: Size<i32, Logical>) -> bool {
    let area = |s: Size<i32, Logical>| s.w as i64 * s.h as i64;
    area(new) > area(old)
}

/// One surface of a captured window tree: an Rc-cloned GL texture and where it
/// sits relative to the window's render origin (logical, pre-scale).
struct BakedSurface {
    buffer: TextureBuffer<GlesTexture>,
    location: Point<f64, Logical>,
    src: Rectangle<f64, Logical>,
    dst: Size<i32, Logical>,
}

/// Content textures captured from a window's surface tree while its buffers are
/// still imported. Keyed by root surface id and consumed at teardown so the
/// close animation is independent of Wayland destruction order.
pub(crate) struct ClosePixels {
    surfaces: Vec<BakedSurface>,
    /// Logical bounds of the captured content, relative to the window origin.
    bounds: Rectangle<f64, Logical>,
    /// The window's geometry rect at capture time, in the same surface-local
    /// space as `bounds`. Recorded here because live geometry collapses to zero
    /// the instant a client unmaps, and foot-family terminals unmap before
    /// destroying their toplevel — by teardown there is nothing left to read.
    pub geometry: Rectangle<i32, Logical>,
    /// When these pixels were cloned, so a stale capture can be discarded.
    pub captured_at: Instant,
}

/// How long captured close pixels stay usable.
///
/// The unmap hook fires on *every* null-buffer commit, not just the one before a
/// destroy, and remap invalidation only covers hide-then-reshow. A hide-to-tray
/// app that quits minutes later would otherwise fade minutes-stale pixels in at
/// its old canvas spot. Unmap-then-destroy clients (the foot family) do both in
/// one dispatch cycle, so a tight bound still covers them.
pub(crate) const MAX_CLOSE_PIXEL_AGE: Duration = Duration::from_secs(1);

/// Whether pixels captured at `captured_at` are still fresh enough to animate.
pub(crate) fn close_pixels_fresh(captured_at: Instant, now: Instant) -> bool {
    now.saturating_duration_since(captured_at) <= MAX_CLOSE_PIXEL_AGE
}

/// Clone the already-imported textures of `surface`'s tree. A held
/// `GlesTexture` clone stays renderable for the renderer's lifetime even after
/// the surface's buffers are evicted. Returns `None` for a never-drawn tree
/// (no importable buffers).
pub(crate) fn capture_close_pixels(
    renderer: &mut GlesRenderer,
    surface: &WlSurface,
    geometry: Rectangle<i32, Logical>,
    now: Instant,
) -> Option<ClosePixels> {
    let mut surfaces: Vec<BakedSurface> = Vec::new();
    with_surface_tree_downward(
        surface,
        Point::<f64, Logical>::from((0.0, 0.0)),
        |_, states, location| {
            let mut location = *location;
            let Some(data) = states.data_map.get::<RendererSurfaceStateUserData>() else {
                return TraversalAction::SkipChildren;
            };
            // Bind the view out of the guard first; a guard held in the match
            // scrutinee would live to the arm end (re-entrant-lock hazard).
            let view = data.lock().unwrap().view();
            match view {
                Some(view) => {
                    location += view.offset.to_f64();
                    TraversalAction::DoChildren(location)
                }
                None => TraversalAction::SkipChildren,
            }
        },
        |_, states, location| {
            let mut location = *location;
            let Some(data) = states.data_map.get::<RendererSurfaceStateUserData>() else {
                return;
            };
            let Some(view) = data.lock().unwrap().view() else {
                return;
            };
            location += view.offset.to_f64();
            if import_surface(renderer, states).is_err() {
                return;
            }
            let data = data.lock().unwrap();
            let Some(texture) = data.texture(renderer.context_id()) else {
                return;
            };
            let buffer = TextureBuffer::from_texture(
                renderer,
                texture.clone(),
                data.buffer_scale(),
                data.buffer_transform(),
                None,
            );
            surfaces.push(BakedSurface {
                buffer,
                location,
                src: view.src,
                dst: view.dst,
            });
        },
        |_, _, _| true,
    );
    if surfaces.is_empty() {
        return None;
    }
    let bounds = surfaces
        .iter()
        .map(|s| Rectangle::new(s.location, s.dst.to_f64()))
        .reduce(|a, b| a.merge(b))
        .filter(|b| b.size.w > 0.0 && b.size.h > 0.0)?;
    Some(ClosePixels {
        surfaces,
        bounds,
        geometry,
        captured_at: now,
    })
}

#[cfg(test)]
impl ClosePixels {
    /// A capture with no textures. Pixels need a live renderer, but the stash's
    /// pairing and drop rules are about the stamp and the map — so a headless
    /// test can still seed one and hold the drop sites to account.
    pub(crate) fn empty(geometry: Rectangle<i32, Logical>) -> Self {
        ClosePixels {
            surfaces: Vec::new(),
            bounds: Rectangle::from_size(geometry.size.to_f64()),
            geometry,
            captured_at: Instant::now(),
        }
    }
}

/// How a bake reproduces the compositor chrome its content was drawn with:
/// `bare` windows (fullscreen, or `decoration = "none"`) get no rounding at all,
/// the rest are clipped to `corner_radius` — bottom corners only under an SSD
/// bar, which covers the top edge.
#[derive(Clone, Copy, Debug)]
pub(crate) struct BakeChrome {
    pub bare: bool,
    pub corner_radius: [f32; 4],
}

/// The content a window is about to replace, cloned while a compositor resize
/// holds it frozen. Kept until the client's redraw lands, where it becomes the
/// fading half of the crossfade.
pub(crate) struct ResizeCapture {
    pub pixels: ClosePixels,
    /// The chrome the captured picture wore, resolved when it was captured
    /// rather than when it is baked: the freeze holds one picture still while
    /// fullscreen membership, window rules and config can all move under it.
    pub chrome: BakeChrome,
    /// Which request this content belongs to (see `WindowAnimations`' generation
    /// counter).
    generation: u64,
}

/// Per-window resize captures. Deliberately not the `close_pixels` map, whose
/// semantics are the opposite ones: first-capture-wins, cleared on every new
/// buffer, and only usable for a second.
#[derive(Default)]
pub(crate) struct ResizeCaptures(HashMap<ElementId, ResizeCapture>);

impl ResizeCaptures {
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Replace `id`'s capture with the content of the buffer being retired, so
    /// the crossfade always starts from the last picture that was on screen.
    pub fn stash(
        &mut self,
        id: ElementId,
        pixels: ClosePixels,
        chrome: BakeChrome,
        generation: u64,
    ) {
        self.0.insert(
            id,
            ResizeCapture {
                pixels,
                chrome,
                generation,
            },
        );
    }

    pub fn drop_for(&mut self, id: ElementId) {
        self.0.remove(&id);
    }

    pub fn retain_ids(&mut self, mut resolves: impl FnMut(ElementId) -> bool) {
        self.0.retain(|id, _| resolves(*id));
    }

    /// Take `id`'s capture for a leg that resolved at `generation`. The entry
    /// goes either way: content captured for a superseded request can never
    /// become valid again, and must not be crossfaded into the newer leg (a
    /// commit submitted before a retarget can resolve the request that replaced
    /// it — pre-commit hooks run at submit, the commit itself can be deferred by
    /// a dmabuf fence).
    pub fn take_for(&mut self, id: ElementId, generation: u64) -> Option<ResizeCapture> {
        self.0.remove(&id).filter(|c| c.generation == generation)
    }
}

/// A resized window's old content, fading out over its live new content while
/// the geometry leg plays. Both are drawn into the same interpolated rect, so
/// the exchange reads as one morph rather than a stale stretch and a pop.
pub(crate) struct ResizeCrossfade {
    buffer: TextureBuffer<GlesTexture>,
    /// Texel extent of `buffer`, which is wrapped at scale 1 — see [`texel_src`].
    texels: Size<i32, Physical>,
    /// Which way the leg is going, and so which fade curve to hand over on — see
    /// [`crossfade_out_alpha`]. Fixed at construction: the leg can be retargeted
    /// underneath, but the two pictures this overlay exchanges never change.
    grew: bool,
    /// The overlay's OWN progress. Advanced by the same per-frame factor as the
    /// leg but never seeded from it: a position-only retarget re-seeds the leg
    /// from 0, and reading that would re-opaque a half-faded overlay.
    progress: f64,
}

impl ResizeCrossfade {
    pub fn tick(&mut self, frame_factor: f64) {
        self.progress += (1.0 - self.progress) * frame_factor;
    }

    pub fn is_done(&self) -> bool {
        1.0 - self.progress <= DONE_EPSILON
    }

    /// The overlay element for a window whose live geometry rect starts at
    /// `loc` (fully zoomed physical) and spans `size` (zoomed logical), wrapped
    /// in that window's own animation transform so the old content lands on the
    /// interpolated visual rect exactly as the live content does. `opacity` is
    /// the opacity the window is drawn at — the fade starts from the density
    /// the old picture actually had, not from full.
    pub fn render_element(
        &self,
        loc: Point<f64, Physical>,
        size: Size<i32, Logical>,
        animation: Option<WindowRenderAnimation>,
        opacity: f64,
    ) -> OutputRenderElements {
        let texture = TextureRenderElement::from_texture_buffer(
            loc,
            &self.buffer,
            Some(crossfade_out_alpha(self.progress, self.grew) * opacity as f32),
            Some(super::texel_src(self.texels)),
            Some(size),
            Kind::Unspecified,
        );
        let (origin, offset, scale) = match animation {
            Some(a) => (a.origin, a.offset, a.scale),
            None => (Point::default(), Point::default(), Scale::from(1.0)),
        };
        OutputRenderElements::ClosingWindow(WindowTransformElement::new(
            texture, origin, offset, scale,
        ))
    }
}

/// Flatten captured content into a crossfade overlay. Only the *content* is
/// being exchanged — the live border, shadow and bar keep drawing themselves —
/// so the bake is at most a clip: `CloseChrome` with every other field `None`.
/// Passing no chrome at all is not the same thing, and not what this wants: that
/// bakes the surface tree's full bounds, which for a CSD client includes its own
/// shadow well outside the geometry rect.
///
/// Either way the result covers exactly the geometry rect — the invariant the
/// render side relies on when it draws this texture over the window's
/// interpolated geometry rect. Anything that grew the bake past that rect would
/// silently offset and stretch the overlay.
pub(crate) fn resize_crossfade(
    renderer: &mut GlesRenderer,
    pixels: &ClosePixels,
    new_size: Size<i32, Logical>,
    flatten_scale: f64,
    corner_clip: Option<&GlesTexProgram>,
    chrome: BakeChrome,
) -> Option<ResizeCrossfade> {
    let flatten_scale = flatten_scale.max(1.0);
    let geometry = pixels.geometry.to_f64();
    // A bare window is still cropped to its geometry rect like any other — that
    // is the rect the render side maps onto the visual rect — it only skips the
    // rounding. Accepted artifact: the live path leaves a bare window's overhang
    // (a `decoration = "none"` client's own shadow) on screen and the fade drops
    // it, which is the price of a bake the render side can place from the
    // geometry rect alone.
    let clip = (!chrome.bare).then_some(corner_clip).flatten();
    let (buffer, texels) = match clip {
        // Nothing to clip: `flatten`'s second pass would copy the first at full
        // size for no effect, so bake straight into the geometry rect instead and
        // spare a 4K-sized offscreen (a bare fullscreen bake is the common case).
        None => bake_content(renderer, pixels, geometry, flatten_scale)?,
        Some(clip) => {
            let chrome = CloseChrome {
                geometry,
                corner_radius: chrome.corner_radius,
                corner_clip: Some(clip),
                border_shader: None,
                border_width: 0,
                border_color: [0; 4],
                focused: false,
                shadow_shader: None,
                bar: None,
            };
            let (buffer, _bounds, texels) =
                flatten(renderer, pixels, flatten_scale, Some(&chrome))?;
            (buffer, texels)
        }
    };
    Some(ResizeCrossfade {
        buffer,
        texels,
        grew: leg_grew(pixels.geometry.size, new_size),
        progress: 0.0,
    })
}

/// A short-lived flattened snapshot of a closed window, animated as one texture
/// after the window has left the stage. Canvas-space so mixed-DPI outputs each
/// place it through their own camera/zoom.
pub(crate) struct ClosingSnapshot {
    buffer: TextureBuffer<GlesTexture>,
    /// Texel extent of `buffer`, which is wrapped at scale 1 — see [`texel_src`].
    texels: Size<i32, Physical>,
    /// Full extent in canvas coordinates. Meaningful only for a normal
    /// (non-pinned) close; a pinned/fullscreen snapshot leaves this default and
    /// scopes by `pinned` instead (its rect lives in screen space).
    canvas_rect: Rectangle<f64, Logical>,
    /// `Some((output, screen_rect))` for pinned/fullscreen closes, which render
    /// only on their home output under zoom 1.
    pinned: Option<(String, Rectangle<i32, Logical>)>,
    /// Fade in place at scale 1 (the close→stand-in conversion crossfade).
    alpha_only: bool,
    /// Shrink amplitude for a normal close (`effects.animation_scale`).
    scale_amplitude: f64,
    progress: f64,
}

impl ClosingSnapshot {
    pub fn tick(&mut self, frame_factor: f64) {
        self.progress += (1.0 - self.progress) * frame_factor;
    }

    pub fn is_done(&self) -> bool {
        1.0 - self.progress <= DONE_EPSILON
    }

    /// Canvas bounds for per-output intersection scoping. Callers must check
    /// [`Self::pinned_output`] first — this is unspecified for a pinned snapshot.
    pub fn canvas_rect(&self) -> Rectangle<f64, Logical> {
        self.canvas_rect
    }

    pub fn pinned_output(&self) -> Option<&str> {
        self.pinned.as_ref().map(|(o, _)| o.as_str())
    }
}

/// The live chrome to reproduce in the bake, so the fade starts from the same
/// picture the window had instead of popping to bare square content. Everything
/// here is still resolvable at teardown (the capture runs before
/// `cleanup_surface_state`). Fullscreen windows pass `None`: they have no
/// rounding, border, shadow, or bar live either.
pub(crate) struct CloseChrome<'a> {
    /// Window geometry rect in surface-origin-local logical coords — the clip
    /// reference and the border's inner rect.
    pub geometry: Rectangle<f64, Logical>,
    /// Per-corner radii in logical px, `(tl, tr, br, bl)`; the top pair is 0
    /// under an SSD bar, matching the live clip.
    pub corner_radius: [f32; 4],
    pub corner_clip: Option<&'a GlesTexProgram>,
    pub border_shader: Option<&'a GlesPixelProgram>,
    pub border_width: i32,
    pub border_color: [u8; 4],
    pub focused: bool,
    pub shadow_shader: Option<&'a GlesPixelProgram>,
    /// The still-alive SSD title-bar buffer and its surface-local rect.
    pub bar: Option<(&'a MemoryRenderBuffer, Rectangle<f64, Logical>)>,
}

/// Draw one element into the offscreen currently bound to `frame`, clipped to
/// `phys_size`. Damage is the element's own rect, element-local.
fn bake_draw(
    frame: &mut smithay::backend::renderer::gles::GlesFrame<'_, '_>,
    element: &dyn RenderElement<GlesRenderer>,
    scale: Scale<f64>,
    phys_size: Size<i32, Physical>,
) {
    let src = element.src();
    let dst = element.geometry(scale);
    let Some(mut local) = Rectangle::from_size(phys_size).intersection(dst) else {
        return;
    };
    local.loc -= dst.loc;
    let cache = UserDataMap::new();
    let _ = element.draw(frame, src, dst, &[local], &[], Some(&cache));
}

/// A rasterized snapshot: the offscreen texture, its extent in
/// surface-origin-local logical coords, and its texel size (needed because the
/// wrapper is at buffer scale 1 — see [`texel_src`]).
type Baked = (
    TextureBuffer<GlesTexture>,
    Rectangle<f64, Logical>,
    Size<i32, Physical>,
);

fn phys_size_of(bounds: Rectangle<f64, Logical>, flatten_scale: f64) -> Size<i32, Physical> {
    Size::from((
        (bounds.size.w * flatten_scale).ceil() as i32,
        (bounds.size.h * flatten_scale).ceil() as i32,
    ))
}

/// Rasterize the captured content textures into one offscreen covering exactly
/// `bounds` (surface-origin-local). Surfaces overhanging `bounds` are cropped by
/// the framebuffer, which is how the live corner clip discards a CSD client's
/// own shadow outside geometry.
fn bake_content(
    renderer: &mut GlesRenderer,
    pixels: &ClosePixels,
    bounds: Rectangle<f64, Logical>,
    flatten_scale: f64,
) -> Option<(TextureBuffer<GlesTexture>, Size<i32, Physical>)> {
    let scale = Scale::from(flatten_scale);
    let phys_size = phys_size_of(bounds, flatten_scale);
    if phys_size.w <= 0 || phys_size.h <= 0 {
        return None;
    }
    let buffer_size = phys_size.to_logical(1).to_buffer(1, Transform::Normal);
    let mut texture =
        Offscreen::<GlesTexture>::create_buffer(renderer, Fourcc::Abgr8888, buffer_size).ok()?;
    {
        let mut target = renderer.bind(&mut texture).ok()?;
        let mut frame = renderer
            .render(&mut target, phys_size, Transform::Normal)
            .ok()?;
        let _ = frame.clear(Color32F::TRANSPARENT, &[Rectangle::from_size(phys_size)]);
        // The surface tree walks top-most first; an offscreen is painter's
        // order (bottom paints first), so draw in reverse.
        for surface in pixels.surfaces.iter().rev() {
            let loc = surface.location - bounds.loc;
            let element = TextureRenderElement::from_texture_buffer(
                Point::<f64, Physical>::from((loc.x * flatten_scale, loc.y * flatten_scale)),
                &surface.buffer,
                None,
                Some(surface.src),
                Some(surface.dst),
                Kind::Unspecified,
            );
            bake_draw(&mut frame, &element, scale, phys_size);
        }
        let _ = frame.finish();
    }
    let buffer = TextureBuffer::from_texture(renderer, texture, 1, Transform::Normal, None);
    Some((buffer, phys_size))
}

/// Rasterize the whole closing picture — shadow, border, corner-clipped content,
/// SSD bar — into one offscreen texture. Returns the texture and its extent in
/// surface-origin-local logical coords. Without `chrome` (fullscreen) this is
/// just the content, matching the live bare pass-through.
fn flatten(
    renderer: &mut GlesRenderer,
    pixels: &ClosePixels,
    flatten_scale: f64,
    chrome: Option<&CloseChrome<'_>>,
) -> Option<Baked> {
    let scale = Scale::from(flatten_scale);
    let Some(chrome) = chrome else {
        let (buffer, texels) = bake_content(renderer, pixels, pixels.bounds, flatten_scale)?;
        return Some((buffer, pixels.bounds, texels));
    };

    // Pass 1: content into a texture covering exactly the geometry rect, so the
    // clip shader's buffer-UV → geometry mapping is the identity below.
    let content_rect = chrome.geometry;
    let (content_buffer, content_phys) =
        bake_content(renderer, pixels, content_rect, flatten_scale)?;

    // The body that casts the shadow and wears the border: content plus the SSD
    // bar strip above it, exactly as the live path composes it.
    let body = match chrome.bar {
        Some((_, bar_rect)) => content_rect.merge(bar_rect),
        None => content_rect,
    };
    let shadow_element = chrome.shadow_shader.map(|shader| {
        shaders::bake_shadow_element(
            shader,
            body,
            (chrome.corner_radius[2] + chrome.border_width as f32).max(0.0),
            scale,
        )
    });
    let border_element = chrome.border_shader.and_then(|shader| {
        shaders::bake_border_element(
            shader,
            body,
            chrome.corner_radius[2],
            chrome.border_width,
            chrome.border_color,
            chrome.focused,
            scale,
        )
    });

    // Pass 2 bounds: everything the picture covers.
    let mut bounds = body;
    if let Some((_, area)) = shadow_element {
        bounds = bounds.merge(area.to_f64());
    }
    if let Some((_, area)) = border_element {
        bounds = bounds.merge(area.to_f64());
    }
    let phys_size = phys_size_of(bounds, flatten_scale);
    if phys_size.w <= 0 || phys_size.h <= 0 {
        return None;
    }
    let buffer_size = phys_size.to_logical(1).to_buffer(1, Transform::Normal);
    let mut texture =
        Offscreen::<GlesTexture>::create_buffer(renderer, Fourcc::Abgr8888, buffer_size).ok()?;

    // Elements needing `renderer` are built before the frame borrows it. The
    // content element is placed so its full extent lands on the geometry rect.
    let content_offset = content_rect.loc - bounds.loc;
    let content_element = TextureRenderElement::from_texture_buffer(
        Point::<f64, Physical>::from((
            content_offset.x * flatten_scale,
            content_offset.y * flatten_scale,
        )),
        &content_buffer,
        None,
        Some(super::texel_src(content_phys)),
        Some(content_phys.to_f64().to_logical(scale).to_i32_round()),
        Kind::Unspecified,
    );
    let bar_element = chrome.bar.and_then(|(buf, bar_rect)| {
        let loc = bar_rect.loc - bounds.loc;
        let elem = MemoryRenderBufferRenderElement::from_buffer(
            renderer,
            Point::<f64, Physical>::from((loc.x * flatten_scale, loc.y * flatten_scale)),
            buf,
            None,
            None,
            Some(bar_rect.size.to_i32_round()),
            Kind::Unspecified,
        )
        .ok()?;
        // Same trim as the live bar, so the baked ring keeps its inner stroke.
        let mut dst = painted_rect(Rectangle::new(loc, bar_rect.size), scale);
        // The bar's bottom is the content's top, not a ring edge; smithay's own
        // rounding of both lands on the same row.
        dst.size.h = elem.geometry(scale).size.h;
        Some(TrimmedElement::from_element(elem, dst))
    });
    // Clamp radii like the live clip so a tiny window can't get corners wider
    // than half its side.
    let max_r = ((content_rect.size.w.min(content_rect.size.h) as f32) * 0.5).max(0.0);
    let clamped = chrome.corner_radius.map(|r| r.clamp(0.0, max_r));
    let painted = painted_rect(Rectangle::from_size(content_rect.size), scale);
    let clip_uniforms = vec![
        Uniform::new("aa_scale", flatten_scale as f32),
        Uniform::new(
            "geo_size",
            (
                painted.size.w as f32 / flatten_scale as f32,
                painted.size.h as f32 / flatten_scale as f32,
            ),
        ),
        Uniform::new(
            "corner_radius",
            (clamped[0], clamped[1], clamped[2], clamped[3]),
        ),
        // A scale, not the identity: the content texture covers the rounded-up
        // extent, so mapping the painted rect onto [0,1] leaves the tie column
        // past 1.0 where the shader discards it, as the live clip does.
        Uniform::new(
            "input_to_geo",
            UniformValue::Matrix3x3 {
                matrices: vec![[
                    content_phys.w as f32 / painted.size.w.max(1) as f32,
                    0.0,
                    0.0,
                    0.0,
                    content_phys.h as f32 / painted.size.h.max(1) as f32,
                    0.0,
                    0.0,
                    0.0,
                    1.0,
                ]],
                transpose: false,
            },
        ),
    ];
    {
        let mut target = renderer.bind(&mut texture).ok()?;
        let mut frame = renderer
            .render(&mut target, phys_size, Transform::Normal)
            .ok()?;
        let _ = frame.clear(Color32F::TRANSPARENT, &[Rectangle::from_size(phys_size)]);
        // Painter's order: shadow, border, content, bar (the reverse of the live
        // front-to-back element vec).
        if let Some((ref element, _)) = shadow_element {
            bake_draw(&mut frame, element, scale, phys_size);
        }
        if let Some((ref element, _)) = border_element {
            bake_draw(&mut frame, element, scale, phys_size);
        }
        match chrome.corner_clip {
            Some(clip) => {
                frame.override_default_tex_program(clip.clone(), clip_uniforms);
                bake_draw(&mut frame, &content_element, scale, phys_size);
                frame.clear_tex_program_override();
            }
            None => bake_draw(&mut frame, &content_element, scale, phys_size),
        }
        if let Some(ref element) = bar_element {
            bake_draw(&mut frame, element, scale, phys_size);
        }
        let _ = frame.finish();
    }
    let buffer = TextureBuffer::from_texture(renderer, texture, 1, Transform::Normal, None);
    Some((buffer, bounds, phys_size))
}

/// Build a closing snapshot for a normal (canvas) window from captured pixels.
#[allow(clippy::too_many_arguments)]
pub(crate) fn snapshot_canvas(
    renderer: &mut GlesRenderer,
    pixels: &ClosePixels,
    window_origin: Point<f64, Logical>,
    flatten_scale: f64,
    scale_amplitude: f64,
    alpha_only: bool,
    chrome: Option<&CloseChrome<'_>>,
) -> Option<ClosingSnapshot> {
    let (buffer, bounds, texels) = flatten(renderer, pixels, flatten_scale.max(1.0), chrome)?;
    let canvas_rect = Rectangle::new(window_origin + bounds.loc, bounds.size);
    Some(ClosingSnapshot {
        buffer,
        texels,
        canvas_rect,
        pinned: None,
        alpha_only,
        scale_amplitude,
        progress: 0.0,
    })
}

/// Build a closing snapshot pinned to one output's screen space (pinned or
/// fullscreen closes), rendered there under zoom 1.
#[allow(clippy::too_many_arguments)]
pub(crate) fn snapshot_screen(
    renderer: &mut GlesRenderer,
    pixels: &ClosePixels,
    output: String,
    screen_origin: Point<i32, Logical>,
    flatten_scale: f64,
    scale_amplitude: f64,
    alpha_only: bool,
    chrome: Option<&CloseChrome<'_>>,
) -> Option<ClosingSnapshot> {
    let (buffer, bounds, texels) = flatten(renderer, pixels, flatten_scale.max(1.0), chrome)?;
    let loc = Point::from((
        screen_origin.x + bounds.loc.x.round() as i32,
        screen_origin.y + bounds.loc.y.round() as i32,
    ));
    let screen_rect = Rectangle::new(loc, bounds.size.to_i32_round());
    Some(ClosingSnapshot {
        buffer,
        texels,
        canvas_rect: Rectangle::default(),
        pinned: Some((output, screen_rect)),
        alpha_only,
        scale_amplitude,
        progress: 0.0,
    })
}

impl ClosingSnapshot {
    /// The render element for this snapshot on `output`, or `None` if it does
    /// not belong there.
    fn render_element(
        &self,
        output_name: &str,
        camera: Point<f64, Logical>,
        zoom: f64,
        output_scale: f64,
    ) -> Option<OutputRenderElements> {
        let alpha = fade_out_alpha(self.progress);
        let close_scale = if self.alpha_only {
            1.0
        } else {
            1.0 - (1.0 - self.scale_amplitude) * self.progress
        };

        let (screen_loc, screen_size): (Point<f64, Logical>, Size<f64, Logical>) =
            if let Some((pin_output, screen_rect)) = &self.pinned {
                if pin_output != output_name {
                    return None;
                }
                (screen_rect.loc.to_f64(), screen_rect.size.to_f64())
            } else {
                (
                    Point::from((
                        (self.canvas_rect.loc.x - camera.x) * zoom,
                        (self.canvas_rect.loc.y - camera.y) * zoom,
                    )),
                    Size::from((
                        self.canvas_rect.size.w * zoom,
                        self.canvas_rect.size.h * zoom,
                    )),
                )
            };

        let loc_phys: Point<f64, Physical> =
            Point::from((screen_loc.x * output_scale, screen_loc.y * output_scale));
        let size_phys: Size<f64, Physical> =
            Size::from((screen_size.w * output_scale, screen_size.h * output_scale));
        let center = Point::from((
            loc_phys.x + size_phys.w / 2.0,
            loc_phys.y + size_phys.h / 2.0,
        ));
        let texture = TextureRenderElement::from_texture_buffer(
            loc_phys,
            &self.buffer,
            Some(alpha),
            Some(super::texel_src(self.texels)),
            Some(screen_size.to_i32_round()),
            Kind::Unspecified,
        );
        Some(OutputRenderElements::ClosingWindow(
            WindowTransformElement::new(
                texture,
                center,
                Point::default(),
                Scale::from(close_scale),
            ),
        ))
    }
}

/// Elements for every closing snapshot visible on `output`, top-most first.
pub(crate) fn render_snapshots_for_output(
    snapshots: &[ClosingSnapshot],
    output_name: &str,
    visible: Rectangle<i32, Logical>,
    camera: Point<f64, Logical>,
    zoom: f64,
    output_scale: f64,
) -> Vec<OutputRenderElements> {
    snapshots
        .iter()
        .filter(|s| {
            s.pinned.as_ref().map_or_else(
                || visible.overlaps(s.canvas_rect.to_i32_round()),
                |(o, _)| o == output_name,
            )
        })
        .filter_map(|s| s.render_element(output_name, camera, zoom, output_scale))
        .collect()
}

/// A departing suspended stand-in fading out: either above the live window that
/// adopted its slot, or on its own when dismissed. Rendered via
/// `push_suspended_element` with a decreasing alpha, plus a shrink for a dismiss.
pub(crate) struct StandInFade {
    pub suspended: Rc<SuspendedWindow>,
    pub loc: Point<i32, Logical>,
    /// The stand-in's launching and focused states as displayed when the fade
    /// was created. Frozen because adoption ends the pending relaunch and moves
    /// focus off the stand-in: both feed the chrome cache keys, so resolving
    /// them live would re-rasterize the label and re-color the bar/border, and
    /// the fade would visibly change before fading out.
    pub launching: bool,
    pub focused: bool,
    /// Scale the chrome shrinks toward by the end of the fade. A dismiss uses
    /// `effects.animation_scale`; the adoption crossfade passes `1.0`, keeping
    /// [`Self::shrink_scale`] exactly 1 so its elements stay untransformed.
    pub shrink: f64,
    pub progress: f64,
}

impl StandInFade {
    pub fn tick(&mut self, frame_factor: f64) {
        self.progress += (1.0 - self.progress) * frame_factor;
    }

    pub fn is_done(&self) -> bool {
        1.0 - self.progress <= DONE_EPSILON
    }

    pub fn alpha(&self) -> f32 {
        fade_out_alpha(self.progress)
    }

    /// Current shrink factor. Exactly `1.0` for the whole fade when `shrink` is
    /// `1.0`, so the adoption path never picks up a transform.
    pub fn shrink_scale(&self) -> f64 {
        1.0 - (1.0 - self.shrink) * self.progress.clamp(0.0, 1.0)
    }
}

#[cfg(test)]
mod close_pixel_age_tests {
    use super::*;

    /// A capture consumed in the same dispatch cycle is always fresh — this is the
    /// normal close, and the unmap-then-destroy sequence foot-family terminals use.
    #[test]
    fn a_same_tick_capture_is_fresh() {
        let now = Instant::now();
        assert!(close_pixels_fresh(now, now));
        assert!(close_pixels_fresh(now, now + Duration::from_millis(16),));
    }

    /// A hide-to-tray app that quits long after unmapping must not fade in the
    /// pixels it had back then.
    #[test]
    fn a_capture_older_than_the_bound_is_stale() {
        let captured = Instant::now();
        assert!(!close_pixels_fresh(
            captured,
            captured + MAX_CLOSE_PIXEL_AGE + Duration::from_millis(1),
        ));
        assert!(!close_pixels_fresh(
            captured,
            captured + Duration::from_secs(300),
        ));
        // The bound itself still counts as fresh.
        assert!(close_pixels_fresh(captured, captured + MAX_CLOSE_PIXEL_AGE));
    }
}

#[cfg(test)]
mod resize_capture_tests {
    use super::*;

    fn capture() -> ClosePixels {
        ClosePixels::empty(Rectangle::from_size(Size::from((400, 300))))
    }

    fn chrome() -> BakeChrome {
        BakeChrome {
            bare: true,
            corner_radius: [0.0; 4],
        }
    }

    /// The leg that made the request gets its content, once.
    #[test]
    fn a_matching_capture_is_consumed_exactly_once() {
        let mut captures = ResizeCaptures::default();
        let id = ElementId(1);
        captures.stash(id, capture(), chrome(), 7);
        assert!(captures.take_for(id, 7).is_some(), "the leg's own content");
        assert!(captures.take_for(id, 7).is_none(), "and it is consumed");
    }

    /// Content stashed for a superseded request must never be crossfaded into
    /// the request that replaced it (see [`ResizeCaptures::take_for`]), so a
    /// generation mismatch drops it rather than leaving it to be picked up later.
    #[test]
    fn a_stale_generation_capture_is_dropped_not_consumed() {
        let mut captures = ResizeCaptures::default();
        let id = ElementId(1);
        captures.stash(id, capture(), chrome(), 7);
        assert!(
            captures.take_for(id, 8).is_none(),
            "a newer leg must not wear the superseded request's content"
        );
        assert_eq!(captures.len(), 0, "and the stale capture is gone");
    }

    /// Every commit the freeze spans refreshes the capture, so the fade starts
    /// from the last picture that was on screen rather than the first.
    #[test]
    fn a_refresh_replaces_the_previous_capture() {
        let mut captures = ResizeCaptures::default();
        let id = ElementId(1);
        captures.stash(id, capture(), chrome(), 7);
        captures.stash(id, capture(), chrome(), 7);
        assert_eq!(captures.len(), 1);
    }
}

#[cfg(test)]
mod resize_crossfade_direction_tests {
    use super::*;

    fn size(w: i32, h: i32) -> Size<i32, Logical> {
        Size::from((w, h))
    }

    /// Mirrors [`crossfade_out_alpha`]'s grow/shrink split. Both curves still
    /// start opaque and end clear, so neither end pops.
    #[test]
    fn a_grow_hands_the_old_picture_over_earlier_than_a_shrink() {
        for p in [0.25, 0.5, 0.75] {
            let grow = crossfade_out_alpha(p, true);
            let shrink = crossfade_out_alpha(p, false);
            assert!(grow < shrink, "progress {p}: grow {grow}, shrink {shrink}");
        }
        for grew in [true, false] {
            assert_eq!(crossfade_out_alpha(0.0, grew), 1.0);
            assert_eq!(crossfade_out_alpha(1.0, grew), 0.0);
        }
    }

    /// A tie lands on the shrink curve — arbitrary, but pinned so it stays
    /// decided.
    #[test]
    fn a_mixed_axis_leg_is_decided_by_area() {
        assert!(leg_grew(size(400, 300), size(800, 600)));
        assert!(!leg_grew(size(800, 600), size(400, 300)));
        assert!(leg_grew(size(400, 300), size(200, 700)), "140k > 120k");
        assert!(!leg_grew(size(400, 300), size(600, 150)), "90k < 120k");
        assert!(!leg_grew(size(400, 300), size(300, 400)), "a tie holds");
    }
}

#[cfg(test)]
mod standin_fade_tests {
    use super::*;

    fn fade(shrink: f64, progress: f64) -> StandInFade {
        StandInFade {
            suspended: Rc::new(crate::state::SuspendedWindow::new(
                crate::state::SuspendedId(1),
                Size::from((400, 300)),
                driftwm::desktop_entry::AppIdentity {
                    app_id: "a".into(),
                    desktop_id: "a".into(),
                    display_name: "a".into(),
                },
                driftwm::session::Origin::Explicit,
                false,
            )),
            loc: Point::from((0, 0)),
            launching: false,
            focused: false,
            shrink,
            progress,
        }
    }

    /// The adoption crossfade passes `1.0`, and it has to stay *exactly* 1 at
    /// every progress: any drift would wrap its chrome in a transform and change
    /// what is a purely alpha-only exchange.
    #[test]
    fn an_unshrinking_fade_is_exactly_identity_throughout() {
        for p in [0.0, 0.25, 0.5, 0.75, 1.0] {
            assert_eq!(fade(1.0, p).shrink_scale(), 1.0, "progress {p}");
        }
    }

    /// A dismiss shrinks from 1 down to the configured amplitude, like a close.
    #[test]
    fn a_dismiss_fade_shrinks_to_its_amplitude() {
        assert_eq!(fade(0.95, 0.0).shrink_scale(), 1.0);
        assert_eq!(fade(0.95, 1.0).shrink_scale(), 0.95);
        let mid = fade(0.95, 0.5).shrink_scale();
        assert!(mid < 1.0 && mid > 0.95, "monotonic between the two ({mid})");
    }

    /// Alpha follows the same `1 - p^2` curve real closes use.
    #[test]
    fn a_dismiss_fade_uses_the_close_alpha_curve() {
        assert_eq!(fade(0.95, 0.0).alpha(), 1.0);
        assert_eq!(fade(0.95, 1.0).alpha(), 0.0);
        assert!((fade(0.95, 0.3).alpha() - 0.91).abs() < 1e-6);
    }
}
