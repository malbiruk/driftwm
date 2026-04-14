use std::collections::HashMap;

use smithay::backend::renderer::gles::element::PixelShaderElement;
use smithay::backend::renderer::gles::{GlesPixelProgram, GlesTexProgram, GlesTexture};
use smithay::reexports::wayland_server::backend::ObjectId;
use smithay::utils::{Physical, Size};

use super::CaptureOutputState;

pub type CsdShadowEntry = (
    PixelShaderElement,
    (i32, i32),
    Option<crate::render::ShadowPhysKey>,
);

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
    pub blur_scene_generation: u64,
    pub blur_geometry_generation: u64,
    pub blur_camera_generation: u64,
    pub csd_shadows: HashMap<ObjectId, CsdShadowEntry>,
    pub cached_bg_elements: HashMap<String, PixelShaderElement>,
    pub capture_state: HashMap<String, CaptureOutputState>,
    pub tile_shader: Option<GlesTexProgram>,
    pub cached_tile_bg: HashMap<String, crate::render::TileShaderElement>,
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
            blur_scene_generation: 0,
            blur_geometry_generation: 0,
            blur_camera_generation: 0,
            csd_shadows: HashMap::new(),
            cached_bg_elements: HashMap::new(),
            capture_state: HashMap::new(),
            tile_shader: None,
            cached_tile_bg: HashMap::new(),
        }
    }

    pub fn remove_capture_state(&mut self, output_name: &str) {
        self.capture_state
            .retain(|k, _| !k.ends_with(&format!(":{output_name}")));
    }
}
