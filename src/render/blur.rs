use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::element::texture::TextureRenderElement;
use smithay::backend::renderer::element::{Element, Id, Kind};
use smithay::backend::renderer::gles::{
    GlesError, GlesRenderer, GlesTexProgram, GlesTexture, Uniform, UniformName, UniformType,
};
use smithay::backend::renderer::utils::DamageBag;
use smithay::output::Output;
use smithay::utils::{Buffer, Physical, Rectangle, Size, Transform};
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use super::OutputRenderElements;

static BLUR_DOWN_SRC: &str = include_str!("../shaders/blur_down.glsl");
static BLUR_UP_SRC: &str = include_str!("../shaders/blur_up.glsl");

fn hash_background_elements(
    elements: &[OutputRenderElements],
    window_rect: Rectangle<i32, Physical>,
    region_rects: Option<&[Rectangle<i32, Physical>]>,
) -> u64 {
    let mut hasher = DefaultHasher::new();
    elements.len().hash(&mut hasher);
    for elem in elements {
        elem.id().hash(&mut hasher);
    }
    window_rect.loc.x.hash(&mut hasher);
    window_rect.loc.y.hash(&mut hasher);
    window_rect.size.w.hash(&mut hasher);
    window_rect.size.h.hash(&mut hasher);
    // Hash by content, not Arc identity — a fresh Arc with identical rects
    // shouldn't invalidate the cache.
    if let Some(rects) = region_rects {
        rects.len().hash(&mut hasher);
        for r in rects {
            r.loc.x.hash(&mut hasher);
            r.loc.y.hash(&mut hasher);
            r.size.w.hash(&mut hasher);
            r.size.h.hash(&mut hasher);
        }
    }
    hasher.finish()
}

/// Per-window cached textures for Kawase blur ping-pong passes.
pub struct BlurCache {
    pub texture: GlesTexture,
    pub mask: GlesTexture,
    /// Padded ping-pong pair for the exact per-window blur path. Blurring
    /// exactly the window rect makes edge samples clamp to the border
    /// pixels, smearing the backdrop inward as a bevel-like band (#125),
    /// so the blur runs on a padded crop and only the centre is kept.
    /// Lazy: the shared animated path never needs them, and allocating
    /// them eagerly wastes ~10 MB per frosted window.
    pub pads: Option<(GlesTexture, GlesTexture)>,
    pub pad_size: Size<i32, Physical>,
    pub size: Size<i32, Physical>,
    /// Geometry generation the mask was last captured at. The mask is the
    /// window's alpha shape: it changes with geometry, not with background
    /// ticks, so animated refreshes reuse it instead of re-rendering the
    /// surface into a full-output buffer per window per tick.
    pub mask_geometry_generation: u64,
    pub dirty: bool,
    pub last_geometry_generation: u64,
    pub last_camera_generation: u64,
    pub last_background_hash: u64,
    /// Stable element identity across frames. The damage tracker treats elements
    /// with unknown Ids as fully damaged — a fresh Id per frame defeats caching.
    pub id: Id,
    /// Records damage only when the blur texture is actually recomputed.
    /// Cache-hit frames leave this untouched, so the tracker sees zero damage.
    pub damage_bag: DamageBag<i32, Buffer>,
    /// Force-dirty countdown for the first few frames after creation.
    /// Clients backing surfaces with DMA-BUF (GTK4, fuzzel, swaync) finish
    /// their async texture import a frame or two after the surface is mapped.
    /// If we compute the mask alpha capture before the import lands, the mask
    /// is empty alpha → the multiply zeros the blur → we cache an invisible
    /// blur that persists until something else (camera move, geometry change)
    /// invalidates the cache. Forcing a recompute for the next frame after
    /// creation gives the import time to settle.
    pub force_dirty_frames: u8,
}

impl BlurCache {
    pub fn new(
        renderer: &mut GlesRenderer,
        size: Size<i32, Physical>,
        pad_size: Size<i32, Physical>,
    ) -> Option<Self> {
        use smithay::backend::renderer::Offscreen;
        let buf_size = size.to_logical(1).to_buffer(1, Transform::Normal);
        let t1 =
            Offscreen::<GlesTexture>::create_buffer(renderer, Fourcc::Abgr8888, buf_size).ok()?;
        let t3 =
            Offscreen::<GlesTexture>::create_buffer(renderer, Fourcc::Abgr8888, buf_size).ok()?;
        Some(Self {
            texture: t1,
            mask: t3,
            pads: None,
            pad_size,
            size,
            mask_geometry_generation: u64::MAX,
            dirty: true,
            last_geometry_generation: 0,
            last_camera_generation: 0,
            last_background_hash: 0,
            id: Id::new(),
            damage_bag: DamageBag::new(4),
            force_dirty_frames: 2,
        })
    }

    /// Create the padded ping-pong pair on first use of the exact path.
    pub fn ensure_pads(&mut self, renderer: &mut GlesRenderer) -> bool {
        use smithay::backend::renderer::Offscreen;
        if self.pads.is_some() {
            return true;
        }
        let pad_buf_size = self.pad_size.to_logical(1).to_buffer(1, Transform::Normal);
        let a = Offscreen::<GlesTexture>::create_buffer(renderer, Fourcc::Abgr8888, pad_buf_size);
        let b = Offscreen::<GlesTexture>::create_buffer(renderer, Fourcc::Abgr8888, pad_buf_size);
        if let (Ok(a), Ok(b)) = (a, b) {
            self.pads = Some((a, b));
            true
        } else {
            false
        }
    }

    pub fn resize(
        &mut self,
        renderer: &mut GlesRenderer,
        size: Size<i32, Physical>,
        pad_size: Size<i32, Physical>,
    ) {
        use smithay::backend::renderer::Offscreen;
        let buf_size = size.to_logical(1).to_buffer(1, Transform::Normal);
        if let Ok(t1) =
            Offscreen::<GlesTexture>::create_buffer(renderer, Fourcc::Abgr8888, buf_size)
            && let Ok(t3) =
                Offscreen::<GlesTexture>::create_buffer(renderer, Fourcc::Abgr8888, buf_size)
        {
            self.texture = t1;
            self.mask = t3;
            self.pads = None;
            self.pad_size = pad_size;
            self.size = size;
            self.mask_geometry_generation = u64::MAX;
            self.dirty = true;
            // Stored damage rects are at the old size — drop them; next render reseeds.
            self.damage_bag.reset();
        }
    }
}

/// Per-output shared blurred-background state for `animate_blur`: ping-pong
/// pair plus its refresh throttle. Keyed per output in `RenderCache` —
/// outputs differ in size and render on their own vblanks, so one global
/// entry would thrash (recreate + full re-blur on every size mismatch) the
/// moment a second output exists.
pub struct SharedBlur {
    pub tex_a: GlesTexture,
    pub tex_b: GlesTexture,
    pub size: Size<i32, Physical>,
    pub refreshed_at: Option<std::time::Instant>,
    pub camera_generation: u64,
}

/// Padding around the blur crop so the Kawase reach never touches a texture
/// edge: window-edge samples must see real backdrop, not clamped border
/// pixels. Sized to the blur's worst-case reach at the deepest mip.
fn blur_pad(strength: f32, passes: usize) -> i32 {
    ((strength * (1u32 << (passes + 1)) as f32).ceil() as i32).clamp(16, 128)
}

static BLUR_MASK_SRC: &str = include_str!("../shaders/blur_mask.glsl");

pub(crate) fn compile_blur_shaders(
    renderer: &mut GlesRenderer,
) -> (
    Option<GlesTexProgram>,
    Option<GlesTexProgram>,
    Option<GlesTexProgram>,
) {
    let uniforms = &[
        UniformName::new("u_halfpixel", UniformType::_2f),
        UniformName::new("u_offset", UniformType::_1f),
    ];
    match (
        renderer.compile_custom_texture_shader(BLUR_DOWN_SRC, uniforms),
        renderer.compile_custom_texture_shader(BLUR_UP_SRC, uniforms),
        renderer.compile_custom_texture_shader(BLUR_MASK_SRC, &[]),
    ) {
        (Ok(d), Ok(u), Ok(m)) => (Some(d), Some(u), Some(m)),
        (Err(e), _, _) | (_, Err(e), _) | (_, _, Err(e)) => {
            tracing::error!("Failed to compile blur shaders: {e:?}");
            (None, None, None)
        }
    }
}

/// Run dual Kawase blur passes (downscale then upscale) between two textures.
/// After completion, `tex_a` contains the blurred result.
fn render_blur(
    renderer: &mut GlesRenderer,
    down_shader: &GlesTexProgram,
    up_shader: &GlesTexProgram,
    tex_a: &mut GlesTexture,
    tex_b: &mut GlesTexture,
    offset: f32,
    passes: usize,
) -> Result<(), GlesError> {
    use smithay::backend::renderer::Texture;

    let tex_size = tex_a.size();

    for i in 0..passes {
        blur_pass(
            renderer,
            down_shader,
            tex_a,
            tex_b,
            tex_size,
            offset,
            i,
            passes,
            true,
        )?;
        std::mem::swap(tex_a, tex_b);
    }

    for i in 0..passes {
        blur_pass(
            renderer, up_shader, tex_a, tex_b, tex_size, offset, i, passes, false,
        )?;
        std::mem::swap(tex_a, tex_b);
    }

    // 2*passes swaps (even) → tex_a has the result
    Ok(())
}

/// Single blur pass: render src (tex_a) into target (tex_b) with the given shader.
#[allow(clippy::too_many_arguments)]
fn blur_pass(
    renderer: &mut GlesRenderer,
    shader: &GlesTexProgram,
    tex_a: &GlesTexture,
    tex_b: &mut GlesTexture,
    tex_size: Size<i32, smithay::utils::Buffer>,
    offset: f32,
    i: usize,
    passes: usize,
    downscale: bool,
) -> Result<(), GlesError> {
    use smithay::backend::renderer::{Bind, Color32F, Frame, Renderer};

    let (src_shift, dst_shift) = if downscale {
        (i, i + 1)
    } else {
        (passes - i, passes - i - 1)
    };

    let src_w = (tex_size.w >> src_shift).max(1);
    let src_h = (tex_size.h >> src_shift).max(1);
    let dst_w = (tex_size.w >> dst_shift).max(1);
    let dst_h = (tex_size.h >> dst_shift).max(1);

    // Standard Kawase
    let half_pixel = if downscale {
        [1.0 / src_w as f32, 1.0 / src_h as f32]
    } else {
        [0.5 / src_w as f32, 0.5 / src_h as f32]
    };
    let pass_offset = offset / (1 << src_shift) as f32;

    let dst_phys: Size<i32, Physical> = (dst_w, dst_h).into();
    let src_buf: Rectangle<f64, smithay::utils::Buffer> =
        Rectangle::from_size((src_w as f64, src_h as f64).into());

    let src = tex_a.clone();
    {
        let mut target = renderer.bind(tex_b)?;
        let mut frame = renderer.render(&mut target, dst_phys, Transform::Normal)?;
        frame.clear(Color32F::TRANSPARENT, &[Rectangle::from_size(dst_phys)])?;
        frame.render_texture_from_to(
            &src,
            src_buf,
            Rectangle::from_size(dst_phys),
            &[Rectangle::from_size(dst_phys)],
            &[],
            Transform::Normal,
            1.0,
            Some(shader),
            &[
                Uniform::new("u_halfpixel", half_pixel),
                Uniform::new("u_offset", pass_offset),
            ],
        )?;
        let _ = frame.finish()?;
    }
    Ok(())
}

/// Which element group a blur request belongs to — determines its prefix offset.
#[derive(Clone, Copy)]
pub(crate) enum BlurLayer {
    Overlay,
    Top,
    Pinned,
    Normal,
    Widget,
}

/// Data extracted from a blur request.
pub(crate) struct BlurRequestData {
    pub surface_id: smithay::reexports::wayland_server::backend::ObjectId,
    pub screen_rect: Rectangle<i32, Physical>,
    pub elem_start: usize,
    pub elem_count: usize,
    pub layer: BlurLayer,
    /// Client-requested blur region in mask-local physical coords (origin at
    /// `screen_rect.loc`). `None` = whole-window blur (no client region set).
    /// Empty list = client opted out (handled at the trigger site, never
    /// constructed here).
    pub region_rects: Option<Arc<Vec<Rectangle<i32, Physical>>>>,
}

/// Process blur requests: for each blurred window, render behind-content to FBO,
/// crop the window region, run Kawase blur passes, and insert the result.
#[allow(clippy::too_many_arguments)]
pub(crate) fn process_blur_requests(
    state: &mut crate::state::DriftWm,
    renderer: &mut GlesRenderer,
    output: &Output,
    output_scale: f64,
    all_elements: &mut Vec<OutputRenderElements>,
    blur_requests: &[BlurRequestData],
    overlay_prefix: usize,
    top_prefix: usize,
    pinned_prefix: usize,
    normal_prefix: usize,
    widget_prefix: usize,
    background_start: usize,
) {
    use smithay::backend::renderer::Color32F;
    use smithay::backend::renderer::damage::OutputDamageTracker;
    use smithay::backend::renderer::{Bind, Frame, Offscreen, Renderer};

    let logical_size = crate::state::output_logical_size(output);
    let output_size: Size<i32, Physical> = logical_size.to_physical_precise_round(output_scale);
    let out_buf_size = output_size.to_logical(1).to_buffer(1, Transform::Normal);

    // Shared full-output FBO for behind-content rendering — cached on DriftWm, reused if size matches
    let mut bg_tex = match state.render.blur_bg_fbo.take() {
        Some((tex, cached_size)) if cached_size == output_size => tex,
        _ => {
            let Ok(t) =
                Offscreen::<GlesTexture>::create_buffer(renderer, Fourcc::Abgr8888, out_buf_size)
            else {
                return;
            };
            t
        }
    };

    // bg_tex is sampled below through a padded crop that reaches past the captured
    // backdrop wherever a window sits at an output edge. Mirror-wrap it so the blur
    // reflects real backdrop back across the edge: plain CLAMP streaks the edge
    // row/column, default REPEAT wraps in the opposite side. MIRRORED_REPEAT on an
    // NPOT texture needs GLES 3, so fall back to CLAMP on GLES 2 — streaks, but
    // never a black/incomplete sample.
    {
        use smithay::backend::renderer::gles::ffi;
        let _ = renderer.with_context(|gl| unsafe {
            let gles3 = std::ffi::CStr::from_ptr(gl.GetString(ffi::VERSION) as *const _)
                .to_string_lossy()
                .strip_prefix("OpenGL ES ")
                .and_then(|s| s.chars().next())
                .and_then(|c| c.to_digit(10))
                .is_some_and(|major| major >= 3);
            let wrap = if gles3 {
                ffi::MIRRORED_REPEAT
            } else {
                ffi::CLAMP_TO_EDGE
            } as i32;
            gl.BindTexture(ffi::TEXTURE_2D, bg_tex.tex_id());
            gl.TexParameteri(ffi::TEXTURE_2D, ffi::TEXTURE_WRAP_S, wrap);
            gl.TexParameteri(ffi::TEXTURE_2D, ffi::TEXTURE_WRAP_T, wrap);
            gl.BindTexture(ffi::TEXTURE_2D, 0);
        });
    }

    let down_shader = state.render.blur_down_shader.clone().unwrap();
    let up_shader = state.render.blur_up_shader.clone().unwrap();
    let blur_passes = state.config.effects.blur_radius as usize;
    let blur_strength = state.config.effects.blur_strength as f32;
    let context_id = renderer.context_id();
    let output_name = output.name();
    let geom_gen = state.render.blur_geometry_generation;
    let camera_gen = state
        .render
        .blur_camera_generation
        .get(&output_name)
        .copied()
        .unwrap_or(0);
    // Animated background shaders update per frame but the element Id is stable,
    // so the bg_hash optimisation can't detect the change. Re-blurring per
    // window per frame re-renders the whole scene each time and scales with
    // window count; instead the background is blurred ONCE into a shared
    // full-output texture (throttled to [effects].animate_blur_fps, forced on
    // camera moves) and each window slices its rect out of it. Trade-off: a
    // window overlapping another window frosts only the background beneath.
    // animate_blur_fps == 0 disables the live refresh: the frost is captured
    // once and only recomputed on camera/geometry change, so it freezes over an
    // animated wallpaper instead of re-sampling it every 1/fps (also avoids the
    // 1.0/fps division below).
    let animated_bg =
        state.render.background_is_animated && state.config.effects.animate_blur_fps > 0;
    let mut shared_refreshed = false;
    if animated_bg {
        let min_interval =
            std::time::Duration::from_secs_f64(1.0 / state.config.effects.animate_blur_fps as f64);
        let size_ok = state
            .render
            .shared_blur
            .get(&output_name)
            .is_some_and(|s| s.size == output_size);
        if !size_ok {
            let a =
                Offscreen::<GlesTexture>::create_buffer(renderer, Fourcc::Abgr8888, out_buf_size);
            let b =
                Offscreen::<GlesTexture>::create_buffer(renderer, Fourcc::Abgr8888, out_buf_size);
            if let (Ok(a), Ok(b)) = (a, b) {
                state.render.shared_blur.insert(
                    output_name.clone(),
                    SharedBlur {
                        tex_a: a,
                        tex_b: b,
                        size: output_size,
                        refreshed_at: None,
                        camera_generation: 0,
                    },
                );
            }
        }
        if let Some(mut shared) = state.render.shared_blur.remove(&output_name) {
            let camera_moved = shared.camera_generation != camera_gen;
            let time_due = shared
                .refreshed_at
                .is_none_or(|at| at.elapsed() >= min_interval);
            // Also require the background to have actually ticked since the
            // blur's last refresh: animate_blur_fps is independent of
            // [background] animate_fps, so without this a faster blur
            // throttle re-samples and re-blurs an unchanged background.
            let bg_ticked_since_refresh = match (
                state.render.background_last_animate.get(&output_name),
                shared.refreshed_at,
            ) {
                (Some(bg_t), Some(blur_t)) => *bg_t > blur_t,
                _ => true,
            };
            // Camera moves force a refresh but stay inside the throttle: at
            // frame rate a pan would run a full scene render + full-output
            // blur per frame, which is most of the pan heat.
            let due = time_due && (bg_ticked_since_refresh || camera_moved);
            if due {
                let mut rendered = false;
                if let Ok(mut target) = renderer.bind(&mut shared.tex_a) {
                    let mut dt =
                        OutputDamageTracker::new(output_size, output_scale, Transform::Normal);
                    rendered = dt
                        .render_output(
                            renderer,
                            &mut target,
                            0,
                            &all_elements[background_start.min(all_elements.len())..],
                            [0.0f32, 0.0, 0.0, 1.0],
                        )
                        .is_ok();
                }
                if rendered {
                    let _ = render_blur(
                        renderer,
                        &down_shader,
                        &up_shader,
                        &mut shared.tex_a,
                        &mut shared.tex_b,
                        blur_strength * output_scale as f32,
                        blur_passes,
                    );
                    shared.refreshed_at = Some(std::time::Instant::now());
                    shared.camera_generation = camera_gen;
                    shared_refreshed = true;
                }
            }
            state.render.shared_blur.insert(output_name.clone(), shared);
        }
    }

    // Precompute per-request behind depth (index into all_elements where "below this window" begins)
    let behind_starts: Vec<usize> = blur_requests
        .iter()
        .map(|req| {
            let prefix = match req.layer {
                BlurLayer::Overlay => overlay_prefix,
                BlurLayer::Top => top_prefix,
                BlurLayer::Pinned => pinned_prefix,
                BlurLayer::Normal => normal_prefix,
                BlurLayer::Widget => widget_prefix,
            };
            (prefix + req.elem_start + req.elem_count).min(all_elements.len())
        })
        .collect();

    // ── First pass: create/resize caches, update dirty flags, decide who recomputes ──
    let pad = blur_pad(blur_strength, blur_passes);

    // behind_starts alone is a z-order test: side-by-side windows all read as
    // "stacked" and lose the shared slice. Fall through only when an element
    // below this window (padded by the blur reach) actually intersects it.
    let elem_scale = smithay::utils::Scale::from(output_scale);
    let occluded_by_lower: Vec<bool> = blur_requests
        .iter()
        .enumerate()
        .map(|(i, req)| {
            let mut probe = req.screen_rect;
            probe.loc.x -= pad;
            probe.loc.y -= pad;
            probe.size.w += 2 * pad;
            probe.size.h += 2 * pad;
            let start = behind_starts[i].min(background_start);
            all_elements[start..background_start]
                .iter()
                .any(|e| e.geometry(elem_scale).overlaps(probe))
        })
        .collect();
    let mut needs_recompute: Vec<bool> = Vec::with_capacity(blur_requests.len());
    let mut mask_forced: Vec<bool> = Vec::with_capacity(blur_requests.len());
    for (i, req) in blur_requests.iter().enumerate() {
        let win_size = req.screen_rect.size;
        if win_size.w <= 0 || win_size.h <= 0 {
            needs_recompute.push(false);
            mask_forced.push(false);
            continue;
        }
        let pad_size: Size<i32, Physical> = (win_size.w + 2 * pad, win_size.h + 2 * pad).into();

        let key = (output_name.clone(), req.surface_id.clone());
        if !state.render.blur_cache.contains_key(&key) {
            if let Some(c) = BlurCache::new(renderer, win_size, pad_size) {
                state.render.blur_cache.insert(key.clone(), c);
            } else {
                needs_recompute.push(false);
                mask_forced.push(false);
                continue;
            }
        }
        let cache = state.render.blur_cache.get_mut(&key).unwrap();
        let resized = cache.size != win_size || cache.pad_size != pad_size;
        if resized {
            cache.resize(renderer, win_size, pad_size);
        }

        let bg_hash = hash_background_elements(
            &all_elements[behind_starts[i]..],
            req.screen_rect,
            req.region_rects.as_deref().map(|v| v.as_slice()),
        );
        let background_changed = cache.last_background_hash != bg_hash;
        let geom_changed = cache.last_geometry_generation != geom_gen;
        // Pinned windows are screen-fixed like Top/Overlay: the canvas behind
        // them pans under the camera, so their blur must recompute on camera change.
        let camera_dirty = matches!(
            req.layer,
            BlurLayer::Overlay | BlurLayer::Top | BlurLayer::Pinned
        ) && cache.last_camera_generation != camera_gen;

        // Hold occluded windows while a pan is in flight: their recompute is
        // a scene re-render, and the moving screen rect churns the background
        // hash every frame. Markers stay unconsumed so the pending change
        // fires once on settle. Canvas windows pan together with their
        // backdrop, so the held frost stays visually correct meanwhile.
        let pan_in_flight = state
            .render
            .blur_camera_moved_at
            .get(&output_name)
            .is_some_and(|t| t.elapsed() < std::time::Duration::from_millis(150));
        if animated_bg
            && occluded_by_lower[i]
            && pan_in_flight
            && !resized
            && !camera_dirty
            && cache.force_dirty_frames == 0
        {
            mask_forced.push(false);
            needs_recompute.push(false);
            continue;
        }

        // Occluded windows are excluded from the animated cadence: their
        // frost re-renders the scene behind them, so refreshing N stacked
        // windows costs N scene renders per tick and heat scales with window
        // count. Their frost stays static between camera/geometry changes.
        if background_changed
            || geom_changed
            || camera_dirty
            || (animated_bg && shared_refreshed && !occluded_by_lower[i])
        {
            cache.dirty = true;
        }
        mask_forced.push(cache.force_dirty_frames > 0);
        if cache.force_dirty_frames > 0 {
            cache.dirty = true;
            cache.force_dirty_frames -= 1;
        }
        cache.last_background_hash = bg_hash;
        cache.last_geometry_generation = geom_gen;
        cache.last_camera_generation = camera_gen;

        needs_recompute.push(cache.dirty);
    }

    let mask_shader = state.render.blur_mask_shader.clone();

    // ── Loop 1: re-render bg_tex per depth, crop + blur dirty windows ──
    // Requests are front-to-back so behind_start increases (each successive
    // bg render is a shorter suffix — cheaper). Re-render only when depth changes.
    let mut last_bg_depth: Option<usize> = None;
    for (i, req) in blur_requests.iter().enumerate() {
        if !needs_recompute[i] {
            continue;
        }
        let win_size = req.screen_rect.size;
        if win_size.w <= 0 || win_size.h <= 0 {
            continue;
        }
        let key = (output_name.clone(), req.surface_id.clone());
        let Some(cache) = state.render.blur_cache.get_mut(&key) else {
            continue;
        };

        // The shared slice is only exact when nothing but scene background
        // lies beneath this window; a window that actually overlaps a lower
        // one falls through to the per-window path (throttled by the same
        // shared_refreshed cadence), so lower windows show in its frost.
        // Missing shared textures (GL alloc failure) also fall through —
        // skipping would insert this window's never-rendered texture as an
        // invisible blur.
        if animated_bg
            && !occluded_by_lower[i]
            && let Some(shared) = state.render.shared_blur.get(&output_name)
        {
            // Slice this window's rect out of the shared blurred background.
            // Already blurred full-screen, so edges see real neighbours and
            // no padding is needed.
            let shared_src = shared.tex_a.clone();
            let Ok(mut target) = renderer.bind(&mut cache.texture) else {
                continue;
            };
            let Ok(mut frame) = renderer.render(&mut target, win_size, Transform::Normal) else {
                continue;
            };
            let _ = frame.clear(Color32F::TRANSPARENT, &[Rectangle::from_size(win_size)]);
            let src_rect: Rectangle<f64, smithay::utils::Buffer> = Rectangle::new(
                (req.screen_rect.loc.x as f64, req.screen_rect.loc.y as f64).into(),
                (win_size.w as f64, win_size.h as f64).into(),
            );
            let _ = frame.render_texture_from_to(
                &shared_src,
                src_rect,
                Rectangle::from_size(win_size),
                &[Rectangle::from_size(win_size)],
                &[],
                Transform::Normal,
                1.0,
                None,
                &[],
            );
            let _ = frame.finish();
            continue;
        }

        let behind = behind_starts[i];
        if last_bg_depth != Some(behind) {
            let Ok(mut target) = renderer.bind(&mut bg_tex) else {
                state.render.blur_bg_fbo = Some((bg_tex, output_size));
                return;
            };
            let mut dt = OutputDamageTracker::new(output_size, output_scale, Transform::Normal);
            let _ = dt.render_output(
                renderer,
                &mut target,
                0,
                &all_elements[behind..],
                [0.0f32, 0.0, 0.0, 1.0],
            );
            last_bg_depth = Some(behind);
        }

        // Crop from bg_tex into the padded pair WITH padding: blur samples
        // past the window edge must see real backdrop, not clamped border
        // pixels (the edge-fade bevel of #125).
        let pad_size = cache.pad_size;
        if !cache.ensure_pads(renderer) {
            continue;
        }
        let Some((pad_a, pad_b)) = cache.pads.as_mut() else {
            continue;
        };
        {
            let bg_src = bg_tex.clone();
            let Ok(mut target) = renderer.bind(&mut *pad_a) else {
                continue;
            };
            let Ok(mut frame) = renderer.render(&mut target, pad_size, Transform::Normal) else {
                continue;
            };
            let _ = frame.clear(Color32F::TRANSPARENT, &[Rectangle::from_size(pad_size)]);
            // Sample the whole padded rect; where it reaches past the output,
            // bg_tex's mirror wrap (set above) supplies backdrop instead of a
            // transparent ring the blur would bleed inward.
            let want = Rectangle::<i32, Physical>::new(
                (req.screen_rect.loc.x - pad, req.screen_rect.loc.y - pad).into(),
                pad_size,
            );
            let src_rect: Rectangle<f64, smithay::utils::Buffer> = Rectangle::new(
                (want.loc.x as f64, want.loc.y as f64).into(),
                (pad_size.w as f64, pad_size.h as f64).into(),
            );
            let full = Rectangle::from_size(pad_size);
            let _ = frame.render_texture_from_to(
                &bg_src,
                src_rect,
                full,
                &[full],
                &[],
                Transform::Normal,
                1.0,
                None,
                &[],
            );
            let _ = frame.finish();
        }

        // Run Kawase blur passes on the padded crop
        let offset = blur_strength * output_scale as f32;
        let _ = render_blur(
            renderer,
            &down_shader,
            &up_shader,
            pad_a,
            pad_b,
            offset,
            blur_passes,
        );

        // Keep only the centre: blit the window-sized region back into
        // cache.texture, discarding the padding ring and its edge artifacts.
        {
            let blurred = pad_a.clone();
            let Ok(mut target) = renderer.bind(&mut cache.texture) else {
                continue;
            };
            let Ok(mut frame) = renderer.render(&mut target, win_size, Transform::Normal) else {
                continue;
            };
            let _ = frame.clear(Color32F::TRANSPARENT, &[Rectangle::from_size(win_size)]);
            let src_rect: Rectangle<f64, smithay::utils::Buffer> = Rectangle::new(
                (pad as f64, pad as f64).into(),
                (win_size.w as f64, win_size.h as f64).into(),
            );
            let _ = frame.render_texture_from_to(
                &blurred,
                src_rect,
                Rectangle::from_size(win_size),
                &[Rectangle::from_size(win_size)],
                &[],
                Transform::Normal,
                1.0,
                None,
                &[],
            );
            let _ = frame.finish();
        }
    }

    // ── Loop 2: mask render + apply for all dirty windows (safe to overwrite bg_tex) ──
    for (i, req) in blur_requests.iter().enumerate() {
        if !needs_recompute[i] {
            continue;
        }
        let win_size = req.screen_rect.size;
        if win_size.w <= 0 || win_size.h <= 0 {
            continue;
        }

        let prefix = match req.layer {
            BlurLayer::Overlay => overlay_prefix,
            BlurLayer::Top => top_prefix,
            BlurLayer::Pinned => pinned_prefix,
            BlurLayer::Normal => normal_prefix,
            BlurLayer::Widget => widget_prefix,
        };

        // The mask is the window's alpha shape: it changes with geometry
        // and during the DMA-BUF settle frames, not with background ticks.
        // Recapturing it per animated refresh (full-output render + crop
        // per window per tick) made blur cost scale with window count.
        // Accepted tradeoff: an alpha-only change at constant geometry
        // (subsurface map/unmap, a CSD corner-radius change) doesn't bump
        // `geom_gen`, so the mask stays stale until something else (camera
        // move, resize) invalidates it — rare enough not to special-case.
        let key = (output_name.clone(), req.surface_id.clone());
        let mask_stale = mask_forced[i]
            || state
                .render
                .blur_cache
                .get(&key)
                .is_none_or(|c| c.mask_geometry_generation != geom_gen);

        let surf_start = prefix + req.elem_start;
        let surf_end = (surf_start + req.elem_count).min(all_elements.len());
        if mask_stale {
            // Render surface elements to bg_tex to capture the alpha channel
            // (index_shift is 0 here — element insertion hasn't happened yet)
            let Ok(mut target) = renderer.bind(&mut bg_tex) else {
                continue;
            };
            let mut dt = OutputDamageTracker::new(output_size, output_scale, Transform::Normal);
            let _ = dt.render_output(
                renderer,
                &mut target,
                0,
                &all_elements[surf_start..surf_end],
                [0.0f32, 0.0, 0.0, 0.0],
            );
        }

        let Some(cache) = state.render.blur_cache.get_mut(&key) else {
            continue;
        };

        // Crop surface region into cache.mask. When the client provided a
        // blur region, restrict the blit to those rects via the damage
        // parameter (GLES backend implements it as glScissor). The clear
        // above leaves outside-region pixels at alpha=0; the alpha-multiply
        // pass below then zeros blur there.
        let whole_mask = [Rectangle::from_size(win_size)];
        if mask_stale {
            let bg_src = bg_tex.clone();
            let Ok(mut target) = renderer.bind(&mut cache.mask) else {
                continue;
            };
            let Ok(mut frame) = renderer.render(&mut target, win_size, Transform::Normal) else {
                continue;
            };
            let _ = frame.clear(Color32F::TRANSPARENT, &whole_mask);
            let src_rect: Rectangle<f64, smithay::utils::Buffer> = Rectangle::new(
                (req.screen_rect.loc.x as f64, req.screen_rect.loc.y as f64).into(),
                (win_size.w as f64, win_size.h as f64).into(),
            );
            let damage: &[Rectangle<i32, Physical>] = match &req.region_rects {
                Some(rects) => rects.as_slice(),
                None => &whole_mask,
            };
            let _ = frame.render_texture_from_to(
                &bg_src,
                src_rect,
                Rectangle::from_size(win_size),
                damage,
                &[],
                Transform::Normal,
                1.0,
                None,
                &[],
            );
            let _ = frame.finish();
            cache.mask_geometry_generation = geom_gen;
        }

        // Masking pass — threshold surface alpha, multiply blur by it
        let Some(ref shader) = mask_shader else {
            continue;
        };
        {
            use smithay::backend::renderer::gles::ffi;
            let mask_src = cache.mask.clone();
            let Ok(mut target) = renderer.bind(&mut cache.texture) else {
                continue;
            };
            let Ok(mut frame) = renderer.render(&mut target, win_size, Transform::Normal) else {
                continue;
            };
            let _ = frame.with_context(|gl| unsafe {
                gl.Enable(ffi::BLEND);
                gl.BlendFuncSeparate(ffi::ZERO, ffi::SRC_ALPHA, ffi::ZERO, ffi::SRC_ALPHA);
            });
            let _ = frame.render_texture_from_to(
                &mask_src,
                Rectangle::from_size((win_size.w as f64, win_size.h as f64).into()),
                Rectangle::from_size(win_size),
                &[Rectangle::from_size(win_size)],
                &[],
                Transform::Normal,
                1.0,
                Some(shader),
                &[],
            );
            let _ = frame.with_context(|gl| unsafe {
                gl.BlendFunc(ffi::ONE, ffi::ONE_MINUS_SRC_ALPHA);
            });
            let _ = frame.finish();
        }

        // Blur texture content just changed — advance the damage snapshot so the
        // tracker re-composites the blur element on screen this frame.
        let buf = cache.size.to_logical(1).to_buffer(1, Transform::Normal);
        cache.damage_bag.add([Rectangle::from_size(buf)]);
        cache.dirty = false;
    }

    // ── Insert blur elements for all windows (dirty or cached) ──
    let mut index_shift = 0usize;
    for req in blur_requests.iter() {
        let win_size = req.screen_rect.size;
        if win_size.w <= 0 || win_size.h <= 0 {
            continue;
        }
        let key = (output_name.clone(), req.surface_id.clone());
        let Some(cache) = state.render.blur_cache.get(&key) else {
            continue;
        };

        let prefix = match req.layer {
            BlurLayer::Overlay => overlay_prefix,
            BlurLayer::Top => top_prefix,
            BlurLayer::Pinned => pinned_prefix,
            BlurLayer::Normal => normal_prefix,
            BlurLayer::Widget => widget_prefix,
        };
        let insert_idx = prefix + req.elem_start + req.elem_count + index_shift;
        let insert_idx = insert_idx.min(all_elements.len());
        let blur_elem = TextureRenderElement::from_texture_with_damage(
            cache.id.clone(),
            context_id.clone(),
            req.screen_rect.loc.to_f64(),
            cache.texture.clone(),
            1,
            Transform::Normal,
            None,
            None,
            Some(Size::from((
                (win_size.w as f64 / output_scale) as i32,
                (win_size.h as f64 / output_scale) as i32,
            ))),
            None,
            cache.damage_bag.snapshot(),
            Kind::Unspecified,
        );
        all_elements.insert(insert_idx, OutputRenderElements::Blur(blur_elem));
        index_shift += 1;
    }

    // Cache bg_tex back for next frame
    state.render.blur_bg_fbo = Some((bg_tex, output_size));
}
