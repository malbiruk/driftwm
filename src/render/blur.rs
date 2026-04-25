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

use super::OutputRenderElements;

static BLUR_DOWN_SRC: &str = include_str!("../shaders/blur_down.glsl");
static BLUR_UP_SRC: &str = include_str!("../shaders/blur_up.glsl");

fn hash_background_elements(elements: &[OutputRenderElements], window_rect: Rectangle<i32, Physical>) -> u64 {
    let mut hasher = DefaultHasher::new();
    elements.len().hash(&mut hasher);
    for elem in elements {
        elem.id().hash(&mut hasher);
    }
    window_rect.loc.x.hash(&mut hasher);
    window_rect.loc.y.hash(&mut hasher);
    window_rect.size.w.hash(&mut hasher);
    window_rect.size.h.hash(&mut hasher);
    hasher.finish()
}

/// Per-window cached textures for Kawase blur ping-pong passes.
pub struct BlurCache {
    pub texture: GlesTexture,
    pub scratch: GlesTexture,
    pub mask: GlesTexture,
    pub size: Size<i32, Physical>,
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
}

impl BlurCache {
    pub fn new(renderer: &mut GlesRenderer, size: Size<i32, Physical>) -> Option<Self> {
        use smithay::backend::renderer::Offscreen;
        let buf_size = size.to_logical(1).to_buffer(1, Transform::Normal);
        let t1 = Offscreen::<GlesTexture>::create_buffer(renderer, Fourcc::Abgr8888, buf_size).ok()?;
        let t2 = Offscreen::<GlesTexture>::create_buffer(renderer, Fourcc::Abgr8888, buf_size).ok()?;
        let t3 = Offscreen::<GlesTexture>::create_buffer(renderer, Fourcc::Abgr8888, buf_size).ok()?;
        Some(Self {
            texture: t1, scratch: t2, mask: t3, size,
            dirty: true, last_geometry_generation: 0,
            last_camera_generation: 0, last_background_hash: 0,
            id: Id::new(),
            damage_bag: DamageBag::new(4),
        })
    }

    pub fn resize(&mut self, renderer: &mut GlesRenderer, size: Size<i32, Physical>) {
        use smithay::backend::renderer::Offscreen;
        let buf_size = size.to_logical(1).to_buffer(1, Transform::Normal);
        if let Ok(t1) = Offscreen::<GlesTexture>::create_buffer(renderer, Fourcc::Abgr8888, buf_size)
            && let Ok(t2) = Offscreen::<GlesTexture>::create_buffer(renderer, Fourcc::Abgr8888, buf_size)
            && let Ok(t3) = Offscreen::<GlesTexture>::create_buffer(renderer, Fourcc::Abgr8888, buf_size)
        {
            self.texture = t1;
            self.scratch = t2;
            self.mask = t3;
            self.size = size;
            self.dirty = true;
            // Stored damage rects are at the old size — drop them; next render reseeds.
            self.damage_bag.reset();
        }
    }
}

static BLUR_MASK_SRC: &str = include_str!("../shaders/blur_mask.glsl");

pub(crate) fn compile_blur_shaders(renderer: &mut GlesRenderer) -> (Option<GlesTexProgram>, Option<GlesTexProgram>, Option<GlesTexProgram>) {
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
        blur_pass(renderer, down_shader, tex_a, tex_b, tex_size, offset, i, passes, true)?;
        std::mem::swap(tex_a, tex_b);
    }

    for i in 0..passes {
        blur_pass(renderer, up_shader, tex_a, tex_b, tex_size, offset, i, passes, false)?;
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

    let half_pixel = [1.0 / src_w as f32, 1.0 / src_h as f32];
    let pass_offset = offset / (1 << src_shift) as f32;

    let dst_phys: Size<i32, Physical> = (dst_w, dst_h).into();
    let src_buf: Rectangle<f64, smithay::utils::Buffer> =
        Rectangle::from_size((src_w as f64, src_h as f64).into());

    let src = tex_a.clone();
    {
        let mut target = renderer.bind(tex_b)?;
        let mut frame = renderer.render(&mut target, dst_phys, Transform::Normal)?;
        frame.clear(
            Color32F::TRANSPARENT,
            &[Rectangle::from_size(dst_phys)],
        )?;
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
pub(crate) enum BlurLayer { Overlay, Top, Normal, Widget }

/// Data extracted from a blur request.
pub(crate) struct BlurRequestData {
    pub surface_id: smithay::reexports::wayland_server::backend::ObjectId,
    pub screen_rect: Rectangle<i32, Physical>,
    pub elem_start: usize,
    pub elem_count: usize,
    pub layer: BlurLayer,
    /// Kawase pass count for this window. Derived from per-window rule or global `[effects]`.
    pub passes: usize,
    /// Sample offset multiplier for this window. Derived from per-window rule or global `[effects]`.
    pub strength: f32,
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
    normal_prefix: usize,
    widget_prefix: usize,
) {
    use smithay::backend::renderer::{Bind, Frame, Offscreen, Renderer};
    use smithay::backend::renderer::Color32F;
    use smithay::backend::renderer::damage::OutputDamageTracker;

    let logical_size = crate::state::output_logical_size(output);
    let output_size: Size<i32, Physical> = logical_size.to_physical_precise_round(output_scale);
    let out_buf_size = output_size.to_logical(1).to_buffer(1, Transform::Normal);

    // Shared full-output FBO for behind-content rendering — cached on DriftWm, reused if size matches
    let mut bg_tex = match state.render.blur_bg_fbo.take() {
        Some((tex, cached_size)) if cached_size == output_size => tex,
        _ => {
            let Ok(t) = Offscreen::<GlesTexture>::create_buffer(renderer, Fourcc::Abgr8888, out_buf_size)
            else { return };
            t
        }
    };

    let down_shader = state.render.blur_down_shader.clone().unwrap();
    let up_shader = state.render.blur_up_shader.clone().unwrap();
    // Per-request passes and strength are carried on BlurRequestData; global values are the fallback.
    let context_id = renderer.context_id();
    let geom_gen = state.render.blur_geometry_generation;
    let camera_gen = state.render.blur_camera_generation;
    // Only force per-frame blur recomputation for animated backgrounds when the user
    // explicitly opts in. Default behaviour: blur stays cached until the window or
    // camera actually moves, preventing stutter with GLSL wallpaper shaders.
    let animated_bg = state.render.background_is_animated && state.config.effects.animated_blur;

    // Precompute per-request behind depth (index into all_elements where "below this window" begins)
    let behind_starts: Vec<usize> = blur_requests.iter().map(|req| {
        let prefix = match req.layer {
            BlurLayer::Overlay => overlay_prefix,
            BlurLayer::Top => top_prefix,
            BlurLayer::Normal => normal_prefix,
            BlurLayer::Widget => widget_prefix,
        };
        (prefix + req.elem_start + req.elem_count).min(all_elements.len())
    }).collect();

    // ── First pass: create/resize caches, update dirty flags, decide who recomputes ──
    let mut needs_recompute: Vec<bool> = Vec::with_capacity(blur_requests.len());
    for (i, req) in blur_requests.iter().enumerate() {
        let win_size = req.screen_rect.size;
        if win_size.w <= 0 || win_size.h <= 0 {
            needs_recompute.push(false);
            continue;
        }

        if !state.render.blur_cache.contains_key(&req.surface_id) {
            if let Some(c) = BlurCache::new(renderer, win_size) {
                state.render.blur_cache.insert(req.surface_id.clone(), c);
            } else {
                needs_recompute.push(false);
                continue;
            }
        }
        let cache = state.render.blur_cache.get_mut(&req.surface_id).unwrap();
        if cache.size != win_size {
            cache.resize(renderer, win_size);
        }

        let bg_hash = hash_background_elements(&all_elements[behind_starts[i]..], req.screen_rect);
        let background_changed = cache.last_background_hash != bg_hash;
        let geom_changed = cache.last_geometry_generation != geom_gen;
        let camera_dirty = matches!(req.layer, BlurLayer::Overlay | BlurLayer::Top)
            && cache.last_camera_generation != camera_gen;

        if background_changed || geom_changed || camera_dirty || animated_bg {
            cache.dirty = true;
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
        if !needs_recompute[i] { continue; }
        let win_size = req.screen_rect.size;
        if win_size.w <= 0 || win_size.h <= 0 { continue; }
        let Some(cache) = state.render.blur_cache.get_mut(&req.surface_id) else { continue };

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

        // Crop from bg_tex into cache.texture
        {
            let bg_src = bg_tex.clone();
            let Ok(mut target) = renderer.bind(&mut cache.texture) else { continue };
            let Ok(mut frame) = renderer.render(&mut target, win_size, Transform::Normal) else { continue };
            let _ = frame.clear(Color32F::TRANSPARENT, &[Rectangle::from_size(win_size)]);
            let src_rect: Rectangle<f64, smithay::utils::Buffer> = Rectangle::new(
                (req.screen_rect.loc.x as f64, req.screen_rect.loc.y as f64).into(),
                (win_size.w as f64, win_size.h as f64).into(),
            );
            let _ = frame.render_texture_from_to(
                &bg_src,
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

        // Run Kawase blur passes
        let offset = req.strength * output_scale as f32;
        let _ = render_blur(
            renderer,
            &down_shader,
            &up_shader,
            &mut cache.texture,
            &mut cache.scratch,
            offset,
            req.passes,
        );
    }

    // ── Loop 2: mask render + apply for all dirty windows (safe to overwrite bg_tex) ──
    for (i, req) in blur_requests.iter().enumerate() {
        if !needs_recompute[i] { continue; }
        let win_size = req.screen_rect.size;
        if win_size.w <= 0 || win_size.h <= 0 { continue; }

        let prefix = match req.layer {
            BlurLayer::Overlay => overlay_prefix,
            BlurLayer::Top => top_prefix,
            BlurLayer::Normal => normal_prefix,
            BlurLayer::Widget => widget_prefix,
        };

        // Render surface elements to bg_tex to capture alpha channel
        // index_shift is 0 here — element insertion hasn't happened yet
        let surf_start = prefix + req.elem_start;
        let surf_end = (surf_start + req.elem_count).min(all_elements.len());
        {
            let Ok(mut target) = renderer.bind(&mut bg_tex) else { continue };
            let mut dt = OutputDamageTracker::new(output_size, output_scale, Transform::Normal);
            let _ = dt.render_output(
                renderer,
                &mut target,
                0,
                &all_elements[surf_start..surf_end],
                [0.0f32, 0.0, 0.0, 0.0],
            );
        }

        let Some(cache) = state.render.blur_cache.get_mut(&req.surface_id) else { continue };

        // Crop surface region into cache.mask
        {
            let bg_src = bg_tex.clone();
            let Ok(mut target) = renderer.bind(&mut cache.mask) else { continue };
            let Ok(mut frame) = renderer.render(&mut target, win_size, Transform::Normal) else { continue };
            let _ = frame.clear(Color32F::TRANSPARENT, &[Rectangle::from_size(win_size)]);
            let src_rect: Rectangle<f64, smithay::utils::Buffer> = Rectangle::new(
                (req.screen_rect.loc.x as f64, req.screen_rect.loc.y as f64).into(),
                (win_size.w as f64, win_size.h as f64).into(),
            );
            let _ = frame.render_texture_from_to(
                &bg_src,
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

        // Masking pass — threshold surface alpha, multiply blur by it
        let Some(ref shader) = mask_shader else { continue };
        {
            use smithay::backend::renderer::gles::ffi;
            let mask_src = cache.mask.clone();
            let Ok(mut target) = renderer.bind(&mut cache.texture) else { continue };
            let Ok(mut frame) = renderer.render(&mut target, win_size, Transform::Normal) else { continue };
            let _ = frame.with_context(|gl| unsafe {
                gl.Enable(ffi::BLEND);
                gl.BlendFuncSeparate(
                    ffi::ZERO, ffi::SRC_ALPHA,
                    ffi::ZERO, ffi::SRC_ALPHA,
                );
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
        if win_size.w <= 0 || win_size.h <= 0 { continue; }
        let Some(cache) = state.render.blur_cache.get(&req.surface_id) else { continue };

        let prefix = match req.layer {
            BlurLayer::Overlay => overlay_prefix,
            BlurLayer::Top => top_prefix,
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
