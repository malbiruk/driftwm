use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::element::{Kind, utils::RescaleRenderElement};
use smithay::backend::renderer::gles::{
    GlesRenderer, GlesTexture, Uniform, element::PixelShaderElement,
};
use smithay::output::Output;
use smithay::utils::{Logical, Physical, Point, Rectangle, Size};

use driftwm::config::BackgroundKind;

use super::bridge::GlesBridge;
use super::elements::{OutputRenderElements, TileShaderElement};
use super::shaders::{
    BG_UNIFORMS, compile_textured_bg_shader, compile_tile_bg_mirror_shader, compile_tile_bg_shader,
    compile_wallpaper_bg_shader,
};

/// The per-output canvas background, one variant per background mode. Owns its
/// opaque-region policy (via [`bg_opaque_regions`]) so construction and the
/// per-frame [`BackgroundElement::update`] derive opacity the same way — a
/// `resize` can never silently flip the element back to opaque.
pub struct BackgroundElement {
    kind: BgKind,
    /// Composite with alpha so whatever sits below shows through. Sourced from
    /// the image's alpha channel (textures) or `transparent_shader` (shaders).
    transparent: bool,
}

enum BgKind {
    /// `type = "none"`. A sentinel that emits nothing — present (rather than an
    /// empty `cached_bg`) so the lazy per-frame re-init doesn't keep re-firing.
    None,
    /// Procedural `PixelShaderElement` — dot grid or a `type = "shader"` source.
    Shader(PixelShaderElement),
    /// Image tiled across the canvas (`tile_bg.glsl`), scrolls with the camera.
    Tile(TileShaderElement),
    /// Single image stretched to the viewport (`wallpaper_bg.glsl`), fixed.
    Wallpaper(TileShaderElement),
    /// `type = "shader"` sampling a bound `texture` (`compile_textured_bg_shader`).
    TexturedShader(TileShaderElement),
}

/// Opaque regions for a background covering `area`: the whole area unless it
/// composites with alpha. Single source of truth shared by element construction
/// and the per-frame resize, so the two can't disagree.
fn bg_opaque_regions(
    transparent: bool,
    area: Rectangle<i32, Logical>,
) -> Option<Vec<Rectangle<i32, Logical>>> {
    if transparent { None } else { Some(vec![area]) }
}

/// Per-frame viewport inputs for [`BackgroundElement::update`].
struct BgFrame {
    canvas_area: Rectangle<i32, Logical>,
    output_size: Size<i32, Logical>,
    canvas_w: i32,
    canvas_h: i32,
    camera: Point<f64, Logical>,
    zoom: f64,
    camera_moved: bool,
    zoom_changed: bool,
    uniforms_stale: bool,
    time_secs: f32,
}

impl BackgroundElement {
    /// True for the camera-scrolling tiled image, whose uniforms move on every
    /// camera/zoom change — the winit backend forces a full redraw for it.
    pub fn is_tile(&self) -> bool {
        matches!(self.kind, BgKind::Tile(_))
    }

    /// The render element for this frame, z-ordered as the canvas background.
    pub fn render_element<R: super::renderer::DriftRenderer>(
        &self,
        zoom: f64,
    ) -> Option<OutputRenderElements<R>> {
        let origin = Point::<i32, Physical>::from((0, 0));
        Some(match &self.kind {
            BgKind::None => return None,
            BgKind::Shader(e) => OutputRenderElements::Background(GlesBridge(
                RescaleRenderElement::from_element(e.clone(), origin, zoom),
            )),
            // Tile and textured-shader share the canvas-sized, zoom-rescaled path.
            BgKind::Tile(e) | BgKind::TexturedShader(e) => OutputRenderElements::TileBg(
                RescaleRenderElement::from_element(e.clone(), origin, zoom),
            ),
            // Viewport-fixed: already in output coords, no zoom rescale.
            BgKind::Wallpaper(e) => OutputRenderElements::WallpaperBg(e.clone()),
        })
    }

    /// Resize to the current viewport and refresh the uniforms each mode
    /// consumes. Opacity is re-derived from `self.transparent` every frame, so
    /// it survives the resize regardless of what the previous frame set.
    fn update(&mut self, f: &BgFrame) {
        let transparent = self.transparent;
        match &mut self.kind {
            BgKind::None => {}
            BgKind::Shader(e) => {
                e.resize(f.canvas_area, bg_opaque_regions(transparent, f.canvas_area));
                if f.uniforms_stale {
                    e.update_uniforms(vec![
                        Uniform::new("u_camera", (f.camera.x as f32, f.camera.y as f32)),
                        Uniform::new("u_time", f.time_secs),
                        Uniform::new("u_zoom", f.zoom as f32),
                    ]);
                }
            }
            BgKind::Tile(e) => {
                e.resize(f.canvas_area, bg_opaque_regions(transparent, f.canvas_area));
                if f.camera_moved || f.zoom_changed {
                    e.update_uniforms(vec![
                        Uniform::new("u_camera", (f.camera.x as f32, f.camera.y as f32)),
                        Uniform::new("u_tile_size", (e.tex_w as f32, e.tex_h as f32)),
                        Uniform::new("u_output_size", (f.canvas_w as f32, f.canvas_h as f32)),
                    ]);
                }
            }
            BgKind::Wallpaper(e) => {
                // Viewport-fixed: size to the output (not the canvas), and never
                // push uniforms. A stable CommitCounter across pans/zooms is the
                // whole point of wallpaper mode being cheaper than tile mode —
                // blur and elements above don't get damaged for background reasons.
                let output_area = Rectangle::from_size(f.output_size);
                e.resize(output_area, bg_opaque_regions(transparent, output_area));
            }
            BgKind::TexturedShader(e) => {
                // Scrolls/zooms like the plain shader bg. `u_output_size` co-varies
                // with zoom (= output / zoom), so also refresh on zoom_changed even
                // when the shader reads no camera/zoom/time uniform.
                e.resize(f.canvas_area, bg_opaque_regions(transparent, f.canvas_area));
                if f.uniforms_stale || f.zoom_changed {
                    e.update_uniforms(vec![
                        Uniform::new("u_camera", (f.camera.x as f32, f.camera.y as f32)),
                        Uniform::new("u_time", f.time_secs),
                        Uniform::new("u_zoom", f.zoom as f32),
                        Uniform::new("u_output_size", (f.canvas_w as f32, f.canvas_h as f32)),
                        Uniform::new("u_texture_size", (e.tex_w as f32, e.tex_h as f32)),
                    ]);
                }
            }
        }
    }
}

/// Update the cached background element for the current camera/zoom.
/// Returns (camera_moved, zoom_changed) for the caller's damage logic.
pub fn update_background_element(
    state: &mut crate::state::DriftWm,
    output: &Output,
    cur_camera: Point<f64, Logical>,
    cur_zoom: f64,
    last_rendered_camera: Point<f64, Logical>,
    last_rendered_zoom: f64,
) -> (bool, bool) {
    let camera_moved = cur_camera != last_rendered_camera;
    let zoom_changed = cur_zoom != last_rendered_zoom;
    let output_name = output.name();
    let output_size = crate::state::output_logical_size(output);
    let canvas_w = (output_size.w as f64 / cur_zoom).ceil() as i32;
    let canvas_h = (output_size.h as f64 / cur_zoom).ceil() as i32;
    let canvas_area = Rectangle::from_size((canvas_w, canvas_h).into());

    // Only push uniforms the shader actually consumes — update_uniforms bumps
    // the element's CommitCounter, which would damage the full-screen bg every
    // frame and force re-composition of every element above (blur especially).
    let uniforms_stale = (camera_moved && state.render.background_uses_camera)
        || (zoom_changed && state.render.background_uses_zoom)
        || state.render.background_is_animated;

    let frame = BgFrame {
        canvas_area,
        output_size,
        canvas_w,
        canvas_h,
        camera: cur_camera,
        zoom: cur_zoom,
        camera_moved,
        zoom_changed,
        uniforms_stale,
        time_secs: state.start_time.elapsed().as_secs_f32(),
    };
    if let Some(bg) = state.render.cached_bg.get_mut(&output_name) {
        bg.update(&frame);
    }
    (camera_moved, zoom_changed)
}

/// Compile background shader and/or load tile/wallpaper image.
/// Called at startup and on config reload (lazy re-init).
/// On failure, falls back to `DEFAULT_SHADER` — never leaves background uninitialized.
pub fn init_background(
    state: &mut crate::state::DriftWm,
    renderer: &mut GlesRenderer,
    initial_size: Size<i32, Logical>,
    output_name: &str,
) {
    // Don't reset the background_uses_*/is_animated flags here: on a
    // second-monitor cache hit the shader branch reuses the cached compiled
    // shader and skips re-derivation, so a reset would freeze animated /
    // stall pan-driven shaders on the new output. Each init branch is
    // responsible for setting the flags on its own success path.
    // `None` means this output reused a cached compiled shader and reached no
    // fresh verdict, so the error set by the output that first compiled it must
    // be left untouched (a second-monitor cache hit must not clear it).
    let outcome: Option<Result<(), String>> =
        match state.config.background.kind.clone() {
            BackgroundKind::Tile(path) if is_tiff_path(&path) => Some(
                tile_chunks_or_shader_fallback(state, renderer, initial_size, output_name, &path),
            ),
            BackgroundKind::Tile(path) => texture_or_shader_fallback(
                state,
                renderer,
                initial_size,
                output_name,
                &path,
                TextureBgMode::Tile,
            ),
            BackgroundKind::Wallpaper(path) => texture_or_shader_fallback(
                state,
                renderer,
                initial_size,
                output_name,
                &path,
                TextureBgMode::Wallpaper,
            ),
            // Textured shaders render live — the chunk-bake path can't sample a
            // runtime texture.
            BackgroundKind::Shader {
                path,
                texture: Some(texture),
            } => Some(textured_shader_or_fallback(
                state,
                renderer,
                initial_size,
                output_name,
                &path,
                &texture,
            )),
            BackgroundKind::Shader {
                path,
                texture: None,
            } => shader_no_texture_dispatch(state, renderer, initial_size, output_name, &path),
            BackgroundKind::None => {
                init_none_bg(state, output_name);
                Some(Ok(()))
            }
            BackgroundKind::Default => init_shader_bg(state, renderer, initial_size, output_name),
        };

    match outcome {
        Some(Ok(())) => state.clear_error(crate::state::ErrorSource::Background),
        Some(Err(msg)) => state.set_error(crate::state::ErrorSource::Background, msg),
        None => {}
    }
}

/// `type = "none"`: cache a [`BgKind::None`] sentinel so the lazy re-init stops
/// re-firing. Reset shader-mode flags so a prior animated bg stops forcing
/// per-frame redraws.
fn init_none_bg(state: &mut crate::state::DriftWm, output_name: &str) {
    state.render.background_is_animated = false;
    state.render.background_uses_camera = false;
    state.render.background_uses_zoom = false;
    state.render.cached_bg.insert(
        output_name.to_string(),
        BackgroundElement {
            kind: BgKind::None,
            transparent: false,
        },
    );
}

fn is_tiff_path(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    lower.ends_with(".tif") || lower.ends_with(".tiff")
}

fn tile_chunks_or_shader_fallback(
    state: &mut crate::state::DriftWm,
    renderer: &mut GlesRenderer,
    initial_size: Size<i32, Logical>,
    output_name: &str,
    path: &str,
) -> Result<(), String> {
    match init_tile_chunks_bg(state, renderer, path, output_name) {
        Ok(()) => Ok(()),
        Err(msg) => {
            tracing::error!("{msg}, using default shader");
            init_shader_bg(state, renderer, initial_size, output_name);
            Err(msg)
        }
    }
}

/// Tiles load lazily — first ~5-10 frames after init/reload render blank
/// until the budget fills the visible set (no coarser-LOD fallback cold).
fn init_tile_chunks_bg(
    state: &mut crate::state::DriftWm,
    renderer: &mut GlesRenderer,
    path: &str,
    output_name: &str,
) -> Result<(), String> {
    use crate::render::tile_chunks::BgChunkCache;
    use crate::render::tile_chunks_tiff::TiffSource;

    let source = TiffSource::open(path).map_err(|e| format!("tile bg '{path}': {e}"))?;
    if state.render.chunk_bg_shader.is_none() {
        const SRC: &str = include_str!("../shaders/chunk_bg.glsl");
        state.render.chunk_bg_shader = Some(
            renderer
                .compile_custom_texture_shader(SRC, &[])
                .map_err(|e| format!("tile bg '{path}': chunk_bg shader compile: {e}"))?,
        );
    }
    // Fallback plane reuses `tile_bg.glsl` (shared with single-texture tile
    // mode) so wrap is shader-driven instead of one element per `(kx, ky)`.
    if state.render.tile_shader.is_none() {
        state.render.tile_shader = compile_tile_bg_shader(renderer);
    }
    let chunk_shader = state.render.chunk_bg_shader.as_ref().unwrap().clone();
    let fallback_shader = state
        .render
        .tile_shader
        .as_ref()
        .ok_or_else(|| format!("tile bg '{path}': tile_bg shader compile failed"))?
        .clone();
    let budget_bytes = state.config.background.cache_budget_mb as u64 * 1024 * 1024;
    let cache = BgChunkCache::new_from_tiff(
        source,
        std::path::PathBuf::from(path),
        chunk_shader,
        fallback_shader,
        renderer,
        state.loop_signal.clone(),
        budget_bytes,
    )
    .map_err(|e| format!("tile bg '{path}': {e}"))?;
    // Chunked path manages its own elements + uniforms; clear shader-mode
    // flags so a previously-animated shader bg doesn't keep forcing the
    // background-damage path.
    state.render.background_is_animated = false;
    state.render.background_uses_camera = false;
    state.render.background_uses_zoom = false;
    state
        .render
        .cached_tile_chunks
        .insert(output_name.to_string(), cache);
    Ok(())
}

enum ShaderBakeOutcome {
    /// Eligible and the cache was built for this output.
    Baked,
    /// Not a rigid `u_camera`-only shader — caller renders it live.
    Ineligible,
    /// Eligible but reading/compiling failed — caller renders live + reports.
    Failed(String),
}

/// `cache_shader` dispatch for a `Shader` background. Failed and ineligible
/// both fall through to `init_shader_bg` so the screen is never blank.
fn shader_chunks_or_live(
    state: &mut crate::state::DriftWm,
    renderer: &mut GlesRenderer,
    initial_size: Size<i32, Logical>,
    output_name: &str,
    path: &str,
) -> Option<Result<(), String>> {
    match try_init_shader_chunks(state, renderer, output_name, path) {
        ShaderBakeOutcome::Baked => Some(Ok(())),
        ShaderBakeOutcome::Failed(msg) => {
            init_shader_bg(state, renderer, initial_size, output_name);
            Some(Err(msg))
        }
        ShaderBakeOutcome::Ineligible => init_shader_bg(state, renderer, initial_size, output_name),
    }
}

fn try_init_shader_chunks(
    state: &mut crate::state::DriftWm,
    renderer: &mut GlesRenderer,
    output_name: &str,
    path: &str,
) -> ShaderBakeOutcome {
    use crate::render::ShaderChunkCache;

    let src = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) => return ShaderBakeOutcome::Failed(format!("background shader '{path}': {e}")),
    };
    // Eligible = rigid function of canvas: u_camera present, no u_time/u_zoom.
    // A no-u_camera shader is screen-fixed (already cheap); baking it into
    // canvas chunks would make it wrongly scroll. Parallax isn't detectable
    // here (substring match) and is a documented user footgun — see config docs.
    let uses_camera = references_uniform(&src, "vec2", "u_camera");
    let animated = references_uniform(&src, "float", "u_time");
    let uses_zoom = references_uniform(&src, "float", "u_zoom");
    if !uses_camera || animated || uses_zoom {
        return ShaderBakeOutcome::Ineligible;
    }

    let shader = if let Some(ref cached) = state.render.background_shader {
        cached.clone()
    } else {
        match renderer.compile_custom_pixel_shader(&src, BG_UNIFORMS) {
            Ok(s) => {
                state.render.background_shader = Some(s.clone());
                s
            }
            Err(e) => {
                return ShaderBakeOutcome::Failed(format!(
                    "background shader '{path}': compile error: {e}"
                ));
            }
        }
    };

    if state.render.chunk_bg_shader.is_none() {
        const SRC: &str = include_str!("../shaders/chunk_bg.glsl");
        match renderer.compile_custom_texture_shader(SRC, &[]) {
            Ok(s) => state.render.chunk_bg_shader = Some(s),
            Err(e) => {
                return ShaderBakeOutcome::Failed(format!(
                    "background shader '{path}': chunk_bg compile error: {e}"
                ));
            }
        }
    }
    let chunk_bg = state.render.chunk_bg_shader.as_ref().unwrap().clone();

    let output_scale = state
        .space
        .outputs()
        .find(|o| o.name() == output_name)
        .map(|o| o.current_scale().fractional_scale())
        .unwrap_or(1.0);

    let budget_bytes = state.config.background.cache_budget_mb as u64 * 1024 * 1024;
    // Chunked path manages its own elements + uniforms; clear shader-mode flags
    // so a prior animated/pan shader doesn't keep forcing the bg-damage path.
    state.render.background_is_animated = false;
    state.render.background_uses_camera = false;
    state.render.background_uses_zoom = false;
    state.render.cached_shader_chunks.insert(
        output_name.to_string(),
        ShaderChunkCache::new(shader, chunk_bg, output_scale, budget_bytes),
    );
    ShaderBakeOutcome::Baked
}

/// Try the configured image; on failure fall back to the default shader but
/// report the image error (the image is what the user asked for). Always
/// returns a verdict (the image is loaded fresh per call, never cached).
fn texture_or_shader_fallback(
    state: &mut crate::state::DriftWm,
    renderer: &mut GlesRenderer,
    initial_size: Size<i32, Logical>,
    output_name: &str,
    path: &str,
    mode: TextureBgMode,
) -> Option<Result<(), String>> {
    match try_init_texture_bg(state, renderer, initial_size, output_name, path, mode) {
        Ok(()) => Some(Ok(())),
        Err(msg) => {
            init_shader_bg(state, renderer, initial_size, output_name);
            Some(Err(msg))
        }
    }
}

#[derive(Copy, Clone)]
enum TextureBgMode {
    Tile,
    Wallpaper,
}

/// `Ok` on success. On failure the caller falls back to shader mode; the error
/// string is surfaced on the error bar.
fn try_init_texture_bg(
    state: &mut crate::state::DriftWm,
    renderer: &mut GlesRenderer,
    initial_size: Size<i32, Logical>,
    output_name: &str,
    path: &str,
    mode: TextureBgMode,
) -> Result<(), String> {
    // The `image` crate is built PNG/JPEG-only; TIFF is handled solely in tile mode.
    if matches!(mode, TextureBgMode::Wallpaper) && is_tiff_path(path) {
        return Err(format!(
            "wallpaper '{path}': TIFF isn't supported in wallpaper mode (PNG/JPEG only) \
             — use [background] type = \"tile\" for TIFF images"
        ));
    }

    let (texture, w, h, has_transparency) = load_image_to_texture(renderer, path)?;

    let mirror = matches!(mode, TextureBgMode::Tile) && state.config.background.mirror_tile;
    let shader_slot = match (mode, mirror) {
        (TextureBgMode::Tile, false) => &mut state.render.tile_shader,
        (TextureBgMode::Tile, true) => &mut state.render.tile_mirror_shader,
        (TextureBgMode::Wallpaper, _) => &mut state.render.wallpaper_shader,
    };
    if shader_slot.is_none() {
        *shader_slot = match (mode, mirror) {
            (TextureBgMode::Tile, false) => compile_tile_bg_shader(renderer),
            (TextureBgMode::Tile, true) => compile_tile_bg_mirror_shader(renderer),
            (TextureBgMode::Wallpaper, _) => compile_wallpaper_bg_shader(renderer),
        };
    }
    let Some(shader) = shader_slot.clone() else {
        let kind = match mode {
            TextureBgMode::Tile => "tile",
            TextureBgMode::Wallpaper => "wallpaper",
        };
        tracing::error!("{kind} shader compilation failed, using default shader");
        return Err(format!("background: {kind} shader failed to compile"));
    };

    let area = Rectangle::from_size(initial_size);
    let uniforms = match mode {
        TextureBgMode::Tile => vec![
            Uniform::new("u_camera", (0.0f32, 0.0f32)),
            Uniform::new("u_tile_size", (w as f32, h as f32)),
            Uniform::new(
                "u_output_size",
                (initial_size.w as f32, initial_size.h as f32),
            ),
        ],
        // Wallpaper shader has no camera/zoom/time uniforms — image stretches to v_coords [0,1].
        TextureBgMode::Wallpaper => vec![],
    };
    let transparent = has_transparency;
    let elem = TileShaderElement::new(
        shader,
        texture,
        w,
        h,
        area,
        bg_opaque_regions(transparent, area),
        1.0,
        uniforms,
        Kind::Unspecified,
    );
    let kind = match mode {
        TextureBgMode::Tile => BgKind::Tile(elem),
        TextureBgMode::Wallpaper => BgKind::Wallpaper(elem),
    };
    state.render.cached_bg.insert(
        output_name.to_string(),
        BackgroundElement { kind, transparent },
    );
    // Clear stale flags from a prior shader-mode bg — otherwise they'd
    // force every-frame redraws or push uniforms into a texture program
    // that doesn't declare them.
    state.render.background_is_animated = false;
    state.render.background_uses_camera = false;
    state.render.background_uses_zoom = false;
    Ok(())
}

/// Compile a user shader as a texture shader and bind the configured image so
/// it can sample `tex`. On any failure (shader read/compile or image load) fall
/// back to the dot grid — not the user's source, which samples an unbound `tex`
/// and would just draw black.
fn textured_shader_or_fallback(
    state: &mut crate::state::DriftWm,
    renderer: &mut GlesRenderer,
    initial_size: Size<i32, Logical>,
    output_name: &str,
    path: &str,
    texture: &str,
) -> Result<(), String> {
    match try_init_textured_shader_bg(state, renderer, initial_size, output_name, path, texture) {
        Ok(()) => Ok(()),
        Err(msg) => {
            init_default_shader_bg(state, renderer, initial_size, output_name);
            Err(msg)
        }
    }
}

/// Compiled per output (no shared program slot), so it always returns a fresh
/// verdict — never the cache-hit `None` (see the top of [`init_background`]).
fn try_init_textured_shader_bg(
    state: &mut crate::state::DriftWm,
    renderer: &mut GlesRenderer,
    initial_size: Size<i32, Logical>,
    output_name: &str,
    path: &str,
    texture: &str,
) -> Result<(), String> {
    let src =
        std::fs::read_to_string(path).map_err(|e| format!("background shader '{path}': {e}"))?;
    let shader = compile_textured_bg_shader(renderer, &src)
        .map_err(|e| format!("background shader '{path}': {e}"))?;
    let (tex, w, h, _) = load_image_to_texture(renderer, texture)?;

    let area = Rectangle::from_size(initial_size);
    let time_secs = state.start_time.elapsed().as_secs_f32();
    let uniforms = vec![
        Uniform::new("u_camera", (0.0f32, 0.0f32)),
        Uniform::new("u_time", time_secs),
        Uniform::new("u_zoom", 1.0f32),
        Uniform::new(
            "u_output_size",
            (initial_size.w as f32, initial_size.h as f32),
        ),
        Uniform::new("u_texture_size", (w as f32, h as f32)),
    ];
    // A shader's output alpha (not the texture's) decides transparency, and a
    // shader is un-inspectable — so an explicit flag, not autodetect.
    let transparent = state.config.background.transparent_shader;
    let elem = TileShaderElement::new(
        shader,
        tex,
        w,
        h,
        area,
        bg_opaque_regions(transparent, area),
        1.0,
        uniforms,
        Kind::Unspecified,
    );

    state.render.background_is_animated = references_uniform(&src, "float", "u_time");
    state.render.background_uses_camera = references_uniform(&src, "vec2", "u_camera");
    state.render.background_uses_zoom = references_uniform(&src, "float", "u_zoom");
    state.render.cached_bg.insert(
        output_name.to_string(),
        BackgroundElement {
            kind: BgKind::TexturedShader(elem),
            transparent,
        },
    );
    Ok(())
}

/// Returns `(texture, width, height, has_transparency)`. `has_transparency`
/// drops the opaque fast path so sub-255-alpha pixels blend over the layer below.
fn load_image_to_texture(
    renderer: &mut GlesRenderer,
    path: &str,
) -> Result<(GlesTexture, i32, i32, bool), String> {
    use smithay::backend::renderer::ImportMem;
    use smithay::utils::Buffer;

    let img = match image::open(path) {
        Ok(img) => img.into_rgba8(),
        Err(e) => {
            tracing::error!("Failed to load image {path}: {e}, using default shader");
            return Err(format!("background image '{path}': {e}"));
        }
    };
    let (w, h) = img.dimensions();
    let raw = img.into_raw();
    let has_transparency = raw.chunks_exact(4).any(|px| px[3] < 255);
    match renderer.import_memory(
        &raw,
        Fourcc::Abgr8888,
        Size::<i32, Buffer>::from((w as i32, h as i32)),
        false,
    ) {
        Ok(texture) => Ok((texture, w as i32, h as i32, has_transparency)),
        Err(e) => {
            tracing::error!("Failed to upload texture from {path}: {e}, using default shader");
            Err(format!(
                "background image '{path}': upload failed (image likely too large) — \
                 gigapixel wallpapers need a tiled pyramidal TIFF"
            ))
        }
    }
}

/// `None` on a cache hit (no fresh verdict — the prior compile's error state
/// stands); otherwise `Ok`/`Err` for a user shader that read+compiled or
/// failed. The built-in default shader always yields `Ok`.
fn init_shader_bg(
    state: &mut crate::state::DriftWm,
    renderer: &mut GlesRenderer,
    initial_size: Size<i32, Logical>,
    output_name: &str,
) -> Option<Result<(), String>> {
    // Reuse cached shader if already compiled (avoids redundant GPU work
    // when multiple outputs each need a background element).
    let mut outcome: Option<Result<(), String>> = None;
    let shader = if let Some(ref cached) = state.render.background_shader {
        cached.clone()
    } else {
        let mut err: Option<String> = None;
        let shader_source = match &state.config.background.kind {
            BackgroundKind::Shader { path, .. } => match std::fs::read_to_string(path) {
                Ok(src) => src,
                Err(e) => {
                    tracing::error!("Failed to read shader {path}: {e}, using default");
                    err = Some(format!("background shader '{path}': {e}"));
                    driftwm::config::DEFAULT_SHADER.to_string()
                }
            },
            _ => driftwm::config::DEFAULT_SHADER.to_string(),
        };

        let compiled = match renderer.compile_custom_pixel_shader(&shader_source, BG_UNIFORMS) {
            Ok(s) => s,
            Err(e) => {
                tracing::error!("Failed to compile shader: {e}, using default");
                err.get_or_insert_with(|| format!("background shader: compile error: {e}"));
                renderer
                    .compile_custom_pixel_shader(driftwm::config::DEFAULT_SHADER, BG_UNIFORMS)
                    .expect("Default shader must compile")
            }
        };

        state.render.background_is_animated = references_uniform(&shader_source, "float", "u_time");
        state.render.background_uses_camera =
            references_uniform(&shader_source, "vec2", "u_camera");
        state.render.background_uses_zoom = references_uniform(&shader_source, "float", "u_zoom");
        state.render.background_shader = Some(compiled.clone());
        outcome = Some(err.map_or(Ok(()), Err));
        compiled
    };

    let area = Rectangle::from_size(initial_size);
    let transparent = state.config.background.transparent_shader;
    let time_secs = state.start_time.elapsed().as_secs_f32();
    let elem = PixelShaderElement::new(
        shader,
        area,
        bg_opaque_regions(transparent, area),
        1.0,
        vec![
            Uniform::new("u_camera", (0.0f32, 0.0f32)),
            Uniform::new("u_time", time_secs),
            Uniform::new("u_zoom", 1.0f32),
        ],
        Kind::Unspecified,
    );
    state.render.cached_bg.insert(
        output_name.to_string(),
        BackgroundElement {
            kind: BgKind::Shader(elem),
            transparent,
        },
    );

    outcome
}

/// Dispatch a `type = "shader"` background with no `texture`. If the source
/// samples `tex`, the user meant to configure a `texture` but didn't — that
/// would render black, so report it and fall back to the dot grid instead of
/// silently compiling a tex-sampling shader with no texture bound. Otherwise
/// take the normal cached/live shader path.
fn shader_no_texture_dispatch(
    state: &mut crate::state::DriftWm,
    renderer: &mut GlesRenderer,
    initial_size: Size<i32, Logical>,
    output_name: &str,
    path: &str,
) -> Option<Result<(), String>> {
    if let Ok(src) = std::fs::read_to_string(path)
        && references_uniform(&src, "sampler2D", "tex")
    {
        init_default_shader_bg(state, renderer, initial_size, output_name);
        return Some(Err(format!(
            "background shader '{path}': samples `tex` but no `texture` is set — \
             add a `texture` path under [background]"
        )));
    }
    // The chunk-bake cache bakes the shader into opaque canvas textures, which
    // can't carry transparency — so `transparent_shader` forces the live path.
    if state.config.background.cache_shader && !state.config.background.transparent_shader {
        shader_chunks_or_live(state, renderer, initial_size, output_name, path)
    } else {
        init_shader_bg(state, renderer, initial_size, output_name)
    }
}

/// Render the built-in dot grid, ignoring the configured shader source — the
/// fallback when a `texture` shader can't be honored. Mirrors the cross-output
/// shader caching in [`init_shader_bg`].
fn init_default_shader_bg(
    state: &mut crate::state::DriftWm,
    renderer: &mut GlesRenderer,
    initial_size: Size<i32, Logical>,
    output_name: &str,
) {
    let shader = if let Some(ref cached) = state.render.background_shader {
        cached.clone()
    } else {
        let src = driftwm::config::DEFAULT_SHADER;
        let compiled = renderer
            .compile_custom_pixel_shader(src, BG_UNIFORMS)
            .expect("Default shader must compile");
        state.render.background_is_animated = references_uniform(src, "float", "u_time");
        state.render.background_uses_camera = references_uniform(src, "vec2", "u_camera");
        state.render.background_uses_zoom = references_uniform(src, "float", "u_zoom");
        state.render.background_shader = Some(compiled.clone());
        compiled
    };

    let area = Rectangle::from_size(initial_size);
    let transparent = state.config.background.transparent_shader;
    let time_secs = state.start_time.elapsed().as_secs_f32();
    let elem = PixelShaderElement::new(
        shader,
        area,
        bg_opaque_regions(transparent, area),
        1.0,
        vec![
            Uniform::new("u_camera", (0.0f32, 0.0f32)),
            Uniform::new("u_time", time_secs),
            Uniform::new("u_zoom", 1.0f32),
        ],
        Kind::Unspecified,
    );
    state.render.cached_bg.insert(
        output_name.to_string(),
        BackgroundElement {
            kind: BgKind::Shader(elem),
            transparent,
        },
    );
}

/// True if `src` declares `uniform <type> <name>` (with optional precision
/// qualifier). Drives the per-uniform damage gating in `update_background_element`.
fn references_uniform(src: &str, type_: &str, name: &str) -> bool {
    ["", "lowp ", "mediump ", "highp "]
        .iter()
        .any(|prec| src.contains(&format!("uniform {prec}{type_} {name}")))
}
