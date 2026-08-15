use std::collections::HashMap;

use smithay::backend::renderer::element::memory::MemoryRenderBuffer;
use smithay::backend::renderer::gles::element::PixelShaderElement;
use smithay::backend::renderer::gles::{GlesPixelProgram, GlesTexProgram};
use smithay::reexports::wayland_server::backend::ObjectId;

use crate::decorations::DecorationKey;

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
    pub blur_geometry_generation: u64,
    /// Wrap mode the blur's backdrop textures are allocated with. A property of
    /// the GL context, resolved on first use because the query behind it is a
    /// `glGetString` plus a string walk that would otherwise run per output per
    /// frame.
    pub blur_wrap_mode: Option<i32>,
    /// Per-output pool of scratch textures for the blur's edge captures. Keyed
    /// by output name: the extents that recur are the ones clipped to *that*
    /// output's bounds.
    pub blur_scratch: HashMap<String, crate::render::BlurScratchPool>,
    /// Shared full-output blurred background: ping-pong pair, blurred once per
    /// refresh and sliced per window, so cost stops scaling with the number of
    /// blurred windows. Also carries the refresh cadence every blurred window
    /// follows, sampler or not. Keyed by output name — outputs differ in size
    /// and render on their own vblanks.
    pub shared_blur: HashMap<String, crate::render::SharedBlur>,
    /// Per-output timestamp of the last animated-background uniform push
    /// ([background] animate_fps). Keyed by output name: a single global
    /// stamp would let one output's render satisfy the interval and starve
    /// the others on multi-monitor setups.
    pub background_last_animate: HashMap<String, std::time::Instant>,
    /// A one-shot tick timer is armed for the next animation frame. Without
    /// it the capped animation only advances alongside other redraws.
    pub background_tick_armed: bool,
    pub shadow_cache: HashMap<DecorationKey, ShadowCacheEntry>,
    pub border_cache: HashMap<DecorationKey, BorderCacheEntry>,
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
    /// Solid-colour strip buffers for other outputs' viewport outlines, per
    /// output. Rebuilding them each frame minted a fresh element `Id` every
    /// frame, which re-damaged every outline and made the blur's background
    /// hash differ every frame. Keyed by colour and extent only — see
    /// [`crate::render::OutlineBufferKey`].
    pub cached_outlines:
        HashMap<String, HashMap<crate::render::OutlineBufferKey, MemoryRenderBuffer>>,
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
            blur_geometry_generation: 0,
            blur_wrap_mode: None,
            blur_scratch: HashMap::new(),
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
            cached_outlines: HashMap::new(),
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

    /// Destroy the per-output chunk caches (shader-bake + gigapixel TIFF). For
    /// identity changes only — output disconnect/remap, scale or transform
    /// changes, config reload — where the cache's own geometry is what went
    /// stale. `compose_frame` rebuilds it synchronously on the first frame that
    /// draws the canvas, which for a gigapixel TIFF is a whole-LOD decode and
    /// six thread spawns; use [`Self::shrink_background_for_fullscreen`] for
    /// the transient case.
    pub fn remove_background_chunks(&mut self, output_name: &str) {
        self.cached_tile_chunks.remove(output_name);
        self.cached_shader_chunks.remove(output_name);
    }

    /// Free the bulk of the chunk caches while a fullscreen window conceals the
    /// canvas, keeping the never-blank cover plane and (TIFF) the decoder pool.
    /// The caches stay in their maps, so the frame that uncovers the canvas — an
    /// exit, or the window's opacity dropping below 1.0 — has no rebuild to pay
    /// for, which is where the cost of destroying them lands.
    pub fn shrink_background_for_fullscreen(&mut self, output_name: &str) {
        if let Some(cache) = self.cached_tile_chunks.get_mut(output_name) {
            cache.shrink();
        }
        if let Some(cache) = self.cached_shader_chunks.get_mut(output_name) {
            cache.shrink();
        }
    }

    /// Drop `output_name`'s blur textures: the per-window caches, the shared
    /// backdrop (two full-output textures), and the edge-capture scratch pool.
    /// All are rebuilt on demand, so any path that stops drawing blur on an
    /// output can free them outright.
    pub fn remove_blur_caches(&mut self, output_name: &str) {
        self.shared_blur.remove(output_name);
        self.blur_scratch.remove(output_name);
        self.blur_cache.retain(|(out, _), _| out != output_name);
    }

    /// Drop all per-output GPU state for `output_name`. Called on output
    /// disconnect/remap so a later reconnect re-runs `init_background` instead
    /// of reusing a stale element with the previous geometry.
    pub fn remove_output(&mut self, output_name: &str) {
        self.cached_bg.remove(output_name);
        self.remove_blur_caches(output_name);
        self.background_last_animate.remove(output_name);
        self.cached_error_bar.remove(output_name);
        self.cached_outlines.remove(output_name);
        self.remove_background_chunks(output_name);
        self.remove_capture_state(output_name);
    }
}
