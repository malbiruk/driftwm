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
    /// Keyed by `(output name, surface id)`: a window visible on two outputs needs
    /// an independent blur per output (different scale, size, behind-scene). Keying
    /// by output also lets each output's per-frame prune touch only its own entries.
    pub blur_cache: HashMap<(String, ObjectId), crate::render::BlurCache>,
    pub blur_bg_fbo: Option<(GlesTexture, Size<i32, Physical>)>,
    pub blur_geometry_generation: u64,
    /// Per-output camera-move counter. A single global counter would make one
    /// output's pan force every other output's blur to refresh (bypassing the
    /// animate_blur_fps throttle) even though their cameras never moved.
    pub blur_camera_generation: HashMap<String, u64>,
    /// Shared full-output blurred background for `animate_blur`: ping-pong
    /// pair, blurred once per refresh and sliced per window, so cost stops
    /// scaling with the number of blurred windows. Keyed by output name —
    /// outputs differ in size and render on their own vblanks.
    pub shared_blur: HashMap<String, crate::render::SharedBlur>,
    /// Per-output timestamp of the last animated-background uniform push
    /// ([background] animate_fps). Keyed by output name: a single global
    /// stamp would let one output's render satisfy the interval and starve
    /// the others on multi-monitor setups.
    pub background_last_animate: HashMap<String, std::time::Instant>,
    /// A one-shot tick timer is armed for the next animation frame. Without
    /// it the capped animation only advances alongside other redraws.
    pub background_tick_armed: bool,
    pub shadow_cache: HashMap<ObjectId, ShadowCacheEntry>,
    pub border_cache: HashMap<ObjectId, BorderCacheEntry>,
    /// One element per output for the configured background (shader / tile /
    /// wallpaper / textured shader — the mode lives inside `BackgroundElement`).
    /// Reload and output-disconnect clear it.
    pub cached_bg: HashMap<String, crate::render::BackgroundElement>,
    pub capture_state: HashMap<String, CaptureOutputState>,
    pub tile_shader: Option<GlesTexProgram>,
    /// Tile shader compiled with `MIRROR` — used when `[background] mirror_tile`.
    pub tile_mirror_shader: Option<GlesTexProgram>,
    pub wallpaper_shader: Option<GlesTexProgram>,
    pub cached_error_bar: HashMap<String, crate::render::ErrorBarCache>,
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
            blur_camera_generation: HashMap::new(),
            shared_blur: HashMap::new(),
            background_last_animate: HashMap::new(),
            background_tick_armed: false,
            shadow_cache: HashMap::new(),
            border_cache: HashMap::new(),
            cached_bg: HashMap::new(),
            capture_state: HashMap::new(),
            tile_shader: None,
            tile_mirror_shader: None,
            wallpaper_shader: None,
            cached_error_bar: HashMap::new(),
            chunk_bg_shader: None,
            cached_tile_chunks: HashMap::new(),
            cached_shader_chunks: HashMap::new(),
        }
    }

    pub fn remove_capture_state(&mut self, output_name: &str) {
        self.capture_state
            .retain(|k, _| !k.ends_with(&format!(":{output_name}")));
    }

    /// Drop capture textures unused for the grace period. Otherwise a finished
    /// screenshot/screencast client's offscreen texture (~33 MB at 4K) lingers
    /// until output disconnect. The grace keeps actively-recording clients warm.
    pub fn evict_idle_capture_state(&mut self, now: std::time::Duration) {
        const MAX_IDLE: std::time::Duration = std::time::Duration::from_secs(5);
        self.capture_state
            .retain(|_, cs| now.saturating_sub(cs.last_used) <= MAX_IDLE);
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
        self.cached_bg.remove(output_name);
        self.shared_blur.remove(output_name);
        self.blur_cache.retain(|(out, _), _| out != output_name);
        self.blur_camera_generation.remove(output_name);
        self.background_last_animate.remove(output_name);
        self.cached_error_bar.remove(output_name);
        self.remove_background_chunks(output_name);
        self.remove_capture_state(output_name);
    }
}
