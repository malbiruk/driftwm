use std::collections::HashMap;

use smithay::backend::renderer::gles::element::PixelShaderElement;
use smithay::backend::renderer::gles::{GlesPixelProgram, GlesTexProgram, GlesTexture};
use smithay::reexports::wayland_server::backend::ObjectId;
use smithay::utils::{Physical, Size};

use super::CaptureOutputState;

pub type ShadowCacheEntry = (PixelShaderElement, Option<crate::render::ShadowPhysKey>);

/// Cached GPU resources: compiled shaders, blur textures, background elements, capture state.
pub struct RenderCache {
    pub shadow_shader: Option<GlesPixelProgram>,
    pub corner_clip_shader: Option<GlesTexProgram>,
    pub background_shader: Option<GlesPixelProgram>,
    pub background_is_animated: bool,
    pub blur_down_shader: Option<GlesTexProgram>,
    pub blur_up_shader: Option<GlesTexProgram>,
    pub blur_mask_shader: Option<GlesTexProgram>,
    pub blur_cache: HashMap<ObjectId, crate::render::BlurCache>,
    pub blur_bg_fbo: Option<(GlesTexture, Size<i32, Physical>)>,
    pub blur_geometry_generation: u64,
    pub blur_camera_generation: u64,
    pub shadow_cache: HashMap<ObjectId, ShadowCacheEntry>,
    pub cached_bg_elements: HashMap<String, PixelShaderElement>,
    pub capture_state: HashMap<String, CaptureOutputState>,
    pub tile_shader: Option<GlesTexProgram>,
    pub cached_tile_bg: HashMap<String, crate::render::TileShaderElement>,
    pub wallpaper_shader: Option<GlesTexProgram>,
    pub cached_wallpaper_bg: HashMap<String, crate::render::TileShaderElement>,
}

impl RenderCache {
    pub fn new() -> Self {
        Self {
            shadow_shader: None,
            corner_clip_shader: None,
            background_shader: None,
            background_is_animated: false,
            blur_down_shader: None,
            blur_up_shader: None,
            blur_mask_shader: None,
            blur_cache: HashMap::new(),
            blur_bg_fbo: None,
            blur_geometry_generation: 0,
            blur_camera_generation: 0,
            shadow_cache: HashMap::new(),
            cached_bg_elements: HashMap::new(),
            capture_state: HashMap::new(),
            tile_shader: None,
            cached_tile_bg: HashMap::new(),
            wallpaper_shader: None,
            cached_wallpaper_bg: HashMap::new(),
        }
    }

    pub fn remove_capture_state(&mut self, output_name: &str) {
        self.capture_state
            .retain(|k, _| !k.ends_with(&format!(":{output_name}")));
    }

    /// Drop all per-output GPU state for `output_name`. Called on output
    /// disconnect/remap so a later reconnect re-runs `init_background` instead
    /// of reusing a stale element with the previous geometry.
    pub fn remove_output(&mut self, output_name: &str) {
        self.cached_bg_elements.remove(output_name);
        self.cached_tile_bg.remove(output_name);
        self.cached_wallpaper_bg.remove(output_name);
        self.remove_capture_state(output_name);
    }
}
