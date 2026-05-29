use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::gles::{
    GlesRenderer, GlesTexture, Uniform, element::PixelShaderElement,
};
use smithay::backend::renderer::element::Kind;
use smithay::output::Output;
use smithay::utils::{Logical, Point, Rectangle, Size};

use driftwm::config::BackgroundKind;

use super::elements::TileShaderElement;
use super::shaders::{BG_UNIFORMS, compile_tile_bg_shader, compile_wallpaper_bg_shader};

/// Update the cached background shader element for the current camera/zoom.
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

    if let Some(elem) = state.render.cached_bg_elements.get_mut(&output_name) {
        elem.resize(canvas_area, Some(vec![canvas_area]));
        if uniforms_stale {
            let time_secs = state.start_time.elapsed().as_secs_f32();
            elem.update_uniforms(vec![
                Uniform::new("u_camera", (cur_camera.x as f32, cur_camera.y as f32)),
                Uniform::new("u_time", time_secs),
                Uniform::new("u_zoom", cur_zoom as f32),
            ]);
        }
    } else if let Some(elem) = state.render.cached_tile_bg.get_mut(&output_name) {
        elem.resize(canvas_area, Some(vec![canvas_area]));
        if camera_moved || zoom_changed {
            elem.update_uniforms(vec![
                Uniform::new("u_camera", (cur_camera.x as f32, cur_camera.y as f32)),
                Uniform::new("u_tile_size", (elem.tex_w as f32, elem.tex_h as f32)),
                Uniform::new("u_output_size", (canvas_w as f32, canvas_h as f32)),
            ]);
        }
    } else if let Some(elem) = state.render.cached_wallpaper_bg.get_mut(&output_name) {
        // Viewport-fixed: size to the output (not the canvas), and never push uniforms.
        // Skipping update_uniforms keeps the CommitCounter stable across pans/zooms,
        // which is the whole point of wallpaper mode being cheaper than tile mode —
        // blur and elements above don't get damaged for background reasons.
        let output_area = Rectangle::from_size(output_size);
        elem.resize(output_area, Some(vec![output_area]));
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
    let outcome: Option<Result<(), String>> = match state.config.background.kind.clone() {
        BackgroundKind::Tile(path) if is_tiff_path(&path) => {
            Some(tile_chunks_or_shader_fallback(
                state, renderer, initial_size, output_name, &path,
            ))
        }
        BackgroundKind::Tile(path) => texture_or_shader_fallback(
            state, renderer, initial_size, output_name, &path, TextureBgMode::Tile,
        ),
        BackgroundKind::Wallpaper(path) => texture_or_shader_fallback(
            state, renderer, initial_size, output_name, &path, TextureBgMode::Wallpaper,
        ),
        BackgroundKind::Shader(_) | BackgroundKind::Default => {
            init_shader_bg(state, renderer, initial_size, output_name)
        }
    };

    match outcome {
        Some(Ok(())) => state.clear_error(crate::state::ErrorSource::Background),
        Some(Err(msg)) => state.set_error(crate::state::ErrorSource::Background, msg),
        None => {}
    }
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
    let cache = BgChunkCache::new_from_tiff(source, chunk_shader, fallback_shader, renderer)
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
    let (texture, w, h) = load_image_to_texture(renderer, path)?;

    let shader_slot = match mode {
        TextureBgMode::Tile => &mut state.render.tile_shader,
        TextureBgMode::Wallpaper => &mut state.render.wallpaper_shader,
    };
    if shader_slot.is_none() {
        *shader_slot = match mode {
            TextureBgMode::Tile => compile_tile_bg_shader(renderer),
            TextureBgMode::Wallpaper => compile_wallpaper_bg_shader(renderer),
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
            Uniform::new("u_output_size", (initial_size.w as f32, initial_size.h as f32)),
        ],
        // Wallpaper shader has no camera/zoom/time uniforms — image stretches to v_coords [0,1].
        TextureBgMode::Wallpaper => vec![],
    };
    let elem = TileShaderElement::new(
        shader,
        texture,
        w,
        h,
        area,
        Some(vec![area]),
        1.0,
        uniforms,
        Kind::Unspecified,
    );
    let target = match mode {
        TextureBgMode::Tile => &mut state.render.cached_tile_bg,
        TextureBgMode::Wallpaper => &mut state.render.cached_wallpaper_bg,
    };
    target.insert(output_name.to_string(), elem);
    // Clear stale flags from a prior shader-mode bg — otherwise they'd
    // force every-frame redraws or push uniforms into a texture program
    // that doesn't declare them.
    state.render.background_is_animated = false;
    state.render.background_uses_camera = false;
    state.render.background_uses_zoom = false;
    Ok(())
}

fn load_image_to_texture(
    renderer: &mut GlesRenderer,
    path: &str,
) -> Result<(GlesTexture, i32, i32), String> {
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
    match renderer.import_memory(
        &raw,
        Fourcc::Abgr8888,
        Size::<i32, Buffer>::from((w as i32, h as i32)),
        false,
    ) {
        Ok(texture) => Ok((texture, w as i32, h as i32)),
        Err(e) => {
            tracing::error!("Failed to upload texture from {path}: {e}, using default shader");
            Err(format!("background image '{path}': upload failed: {e}"))
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
            BackgroundKind::Shader(path) => match std::fs::read_to_string(path) {
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
        state.render.background_uses_camera = references_uniform(&shader_source, "vec2", "u_camera");
        state.render.background_uses_zoom = references_uniform(&shader_source, "float", "u_zoom");
        state.render.background_shader = Some(compiled.clone());
        outcome = Some(err.map_or(Ok(()), Err));
        compiled
    };

    let area = Rectangle::from_size(initial_size);
    let time_secs = state.start_time.elapsed().as_secs_f32();
    state.render.cached_bg_elements.insert(output_name.to_string(), PixelShaderElement::new(
        shader,
        area,
        Some(vec![area]),
        1.0,
        vec![
            Uniform::new("u_camera", (0.0f32, 0.0f32)),
            Uniform::new("u_time", time_secs),
            Uniform::new("u_zoom", 1.0f32),
        ],
        Kind::Unspecified,
    ));

    outcome
}

/// True if `src` declares `uniform <type> <name>` (with optional precision
/// qualifier). Drives the per-uniform damage gating in `update_background_element`.
fn references_uniform(src: &str, type_: &str, name: &str) -> bool {
    ["", "lowp ", "mediump ", "highp "]
        .iter()
        .any(|prec| src.contains(&format!("uniform {prec}{type_} {name}")))
}
