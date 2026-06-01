use std::collections::HashMap;

use smithay::backend::renderer::gles::element::PixelShaderElement;
use smithay::backend::renderer::gles::{GlesPixelProgram, GlesTexProgram, GlesTexture};
use smithay::reexports::wayland_server::backend::ObjectId;
use smithay::utils::{Physical, Size};

use super::CaptureOutputState;

pub type ShadowCacheEntry = (PixelShaderElement, Option<crate::render::ShadowPhysKey>);
pub type BorderCacheEntry = (PixelShaderElement, Option<crate::render::BorderPhysKey>);

/// Cached GPU resources: compiled shaders, blur textures, background elements, capture state.
pub struct RenderCache {
    pub shadow_shader: Option<GlesPixelProgram>,
    pub border_shader: Option<GlesPixelProgram>,
    pub corner_clip_shader: Option<GlesTexProgram>,
    pub background_shader: Option<GlesPixelProgram>,
    /// `u_time` is referenced — drives per-frame redraws.
    pub background_is_animated: bool,
    /// `u_camera` is referenced — gates camera-driven uniform pushes so a
    /// shader-mode bg referencing none of u_camera/u_zoom/u_time is as cheap
    /// as wallpaper mode (no per-frame CommitCounter bumps).
    pub background_uses_camera: bool,
    /// `u_zoom` is referenced — gates zoom-driven uniform pushes.
    pub background_uses_zoom: bool,
    pub blur_down_shader: Option<GlesTexProgram>,
    pub blur_up_shader: Option<GlesTexProgram>,
    pub blur_mask_shader: Option<GlesTexProgram>,
    pub blur_cache: HashMap<ObjectId, crate::render::BlurCache>,
    pub blur_bg_fbo: Option<(GlesTexture, Size<i32, Physical>)>,
    pub blur_geometry_generation: u64,
    pub blur_camera_generation: u64,
    pub shadow_cache: HashMap<ObjectId, ShadowCacheEntry>,
    pub border_cache: HashMap<ObjectId, BorderCacheEntry>,
    pub cached_bg_elements: HashMap<String, PixelShaderElement>,
    pub capture_state: HashMap<String, CaptureOutputState>,
    pub tile_shader: Option<GlesTexProgram>,
    pub cached_tile_bg: HashMap<String, crate::render::TileShaderElement>,
    pub wallpaper_shader: Option<GlesTexProgram>,
    pub cached_wallpaper_bg: HashMap<String, crate::render::TileShaderElement>,
    /// Per-output elements for a `type = "shader"` background that samples a
    /// `texture`. Compiled per output (no shared program slot), so reload only
    /// clears this map.
    pub cached_textured_shader_bg: HashMap<String, crate::render::TileShaderElement>,
    pub cached_error_bar: HashMap<String, crate::render::ErrorBarCache>,
    pub cached_help_overlay: HashMap<String, crate::render::HelpOverlayCache>,
    /// Pass-through fragment shader cloned into each `BgChunkCache`.
    pub chunk_bg_shader: Option<GlesTexProgram>,
    pub cached_tile_chunks: HashMap<String, crate::render::BgChunkCache>,
    /// Per-output chunked shader-bake caches (`cache_shader`).
    pub cached_shader_chunks: HashMap<String, crate::render::ShaderChunkCache>,
}

impl RenderCache {
    pub fn new() -> Self {
        Self {
            shadow_shader: None,
            border_shader: None,
            corner_clip_shader: None,
            background_shader: None,
            background_is_animated: false,
            background_uses_camera: false,
            background_uses_zoom: false,
            blur_down_shader: None,
            blur_up_shader: None,
            blur_mask_shader: None,
            blur_cache: HashMap::new(),
            blur_bg_fbo: None,
            blur_geometry_generation: 0,
            blur_camera_generation: 0,
            shadow_cache: HashMap::new(),
            border_cache: HashMap::new(),
            cached_bg_elements: HashMap::new(),
            capture_state: HashMap::new(),
            tile_shader: None,
            cached_tile_bg: HashMap::new(),
            wallpaper_shader: None,
            cached_wallpaper_bg: HashMap::new(),
            cached_textured_shader_bg: HashMap::new(),
            cached_error_bar: HashMap::new(),
            cached_help_overlay: HashMap::new(),
            chunk_bg_shader: None,
            cached_tile_chunks: HashMap::new(),
            cached_shader_chunks: HashMap::new(),
        }
    }

    pub fn remove_capture_state(&mut self, output_name: &str) {
        self.capture_state
            .retain(|k, _| !k.ends_with(&format!(":{output_name}")));
    }

    /// Drop the large per-output chunk caches (shader-bake + gigapixel TIFF),
    /// freeing hundreds of MB of GPU textures. Single-texture tile/wallpaper
    /// caches stay (cheap; re-decoding on exit would hitch). `compose_frame`
    /// lazily rebuilds the chunk caches on the first non-fullscreen frame.
    pub fn remove_background_chunks(&mut self, output_name: &str) {
        self.cached_tile_chunks.remove(output_name);
        self.cached_shader_chunks.remove(output_name);
    }

    /// Drop all per-output GPU state for `output_name`. Called on output
    /// disconnect/remap so a later reconnect re-runs `init_background` instead
    /// of reusing a stale element with the previous geometry.
    pub fn remove_output(&mut self, output_name: &str) {
        self.cached_bg_elements.remove(output_name);
        self.cached_tile_bg.remove(output_name);
        self.cached_wallpaper_bg.remove(output_name);
        self.cached_textured_shader_bg.remove(output_name);
        self.cached_error_bar.remove(output_name);
        self.cached_help_overlay.remove(output_name);
        self.remove_background_chunks(output_name);
        self.remove_capture_state(output_name);
    }
}
