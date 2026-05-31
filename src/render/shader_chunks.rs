//! Chunked shader-bake background. Bakes a static `u_camera`-only GLSL shader
//! into canvas-aligned GPU-texture chunks on demand, so panning samples cached
//! textures instead of re-running the fragment shader every frame.
//!
//! Mirrors the pyramidal-TIFF chunked path ([`super::tile_chunks`]) — same LOD
//! pyramid, same `PixelSnapRescaleElement` display, same LRU eviction — but the
//! per-chunk source is a *synchronous GPU bake* of the shader, not a decoded
//! TIFF tile. The shader is an infinite function of canvas position, so there's
//! no worker pool, no wrap instances, and no finite-image clipping.

use std::collections::{HashMap, HashSet};

use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::damage::OutputDamageTracker;
use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::gles::element::PixelShaderElement;
use smithay::backend::renderer::gles::{
    GlesPixelProgram, GlesRenderer, GlesTexProgram, GlesTexture, Uniform,
};
use smithay::backend::renderer::{Bind, Offscreen, Texture};
use smithay::utils::{Buffer, Logical, Physical, Point, Rectangle, Scale, Size, Transform};

use super::elements::{PixelSnapRescaleElement, TileShaderElement};
use super::tile_chunks::{ChunkMeta, evict_lru_to_budget, pick_lod};

/// LOD-0 chunk span in canvas units (= LOD-0 image pixels). Matches the TIFF
/// path's LOD-0 target so visible-chunk counts per viewport stay comparable.
const BASE_CHUNK_CANVAS: i32 = 1024;

/// Number of LOD levels. LOD k spans `BASE << k` canvas units, so LOD 7 reaches
/// zoom ≈ 1/128 — past driftwm's practical zoom-out.
const N_LODS: u32 = 8;

/// Fine-refinement bakes per frame. The resident coarse cover (see
/// [`ShaderChunkCache::render_elements`]) is baked unbudgeted, but it's the
/// coarsest LOD so it's baked once and reused across pans; this caps only the
/// sharp target-LOD bakes so a fast pan can't stall a frame. Tuned via Tracy.
const BAKE_BUDGET: usize = 2;

pub struct ShaderChunkCache {
    shader: GlesPixelProgram,
    chunk_bg_shader: GlesTexProgram,
    /// Per-LOD chunk span in canvas units: `BASE << lod`.
    chunk_canvas_sizes: Vec<i32>,
    /// Baked-texture resolution per chunk = `BASE * output_scale` (1:1 device
    /// pixels at each LOD's native zoom). Constant across LODs.
    bake_px: i32,
    chunks: HashMap<(u32, i32, i32), GlesTexture>,
    chunk_elements: HashMap<(u32, i32, i32), TileShaderElement>,
    chunk_meta: HashMap<(u32, i32, i32), ChunkMeta>,
    vram_bytes: u64,
    /// LRU ceiling in bytes (`[background] cache_budget_mb`). Each chunk is
    /// `bake_px² * 4` bytes (~4 MB at scale 1.0).
    vram_budget_bytes: u64,
    frame_counter: u64,
    /// True while any visible target-LOD chunk is still unbaked. Drives the
    /// udev loop to re-fire so refinement completes without external damage
    /// (mirrors the TIFF path's `has_pending_loads`).
    pending: bool,
    /// True when the visible working set alone exceeds the budget: fine refine
    /// is skipped (coarse cover only) so the cache stays blurry but stable
    /// instead of thrash-evicting visible chunks every frame. Tracked so the
    /// transition logs once, not per frame.
    degraded: bool,
}

impl ShaderChunkCache {
    pub fn new(
        shader: GlesPixelProgram,
        chunk_bg_shader: GlesTexProgram,
        output_scale: f64,
        vram_budget_bytes: u64,
    ) -> Self {
        let chunk_canvas_sizes = (0..N_LODS).map(|k| BASE_CHUNK_CANVAS << k).collect();
        let bake_px = ((BASE_CHUNK_CANVAS as f64) * output_scale).round().max(1.0) as i32;
        Self {
            shader,
            chunk_bg_shader,
            chunk_canvas_sizes,
            bake_px,
            chunks: HashMap::new(),
            chunk_elements: HashMap::new(),
            chunk_meta: HashMap::new(),
            vram_bytes: 0,
            vram_budget_bytes,
            frame_counter: 0,
            pending: false,
            degraded: false,
        }
    }

    fn n_lods(&self) -> u32 {
        self.chunk_canvas_sizes.len() as u32
    }

    fn chunk_canvas_size_at(&self, lod: u32) -> i32 {
        self.chunk_canvas_sizes[lod as usize]
    }

    pub fn has_pending_bakes(&self) -> bool {
        self.pending
    }

    /// Bake chunk `(lod, cx, cy)` into a `bake_px²` texture by rendering the
    /// shader once with `u_camera` = the chunk's canvas origin and the `size`
    /// uniform = the chunk's canvas span. Goes through the same
    /// `PixelShaderElement` draw path as the live shader, so baked pixels match
    /// live output exactly. On GPU failure the chunk is left uncached, so the
    /// coarse-to-fine resolve falls back to a coarser LOD.
    fn bake_chunk(
        &mut self,
        renderer: &mut GlesRenderer,
        lod: u32,
        cx: i32,
        cy: i32,
        frame: u64,
    ) {
        let chunk_size = self.chunk_canvas_size_at(lod);
        let origin = chunk_origin(cx, cy, chunk_size);
        let bake_px = self.bake_px;
        // Bake scale < 1 at coarse LODs: bake_px texels cover chunk_size canvas
        // units. The PixelShaderElement area is in canvas (logical) units, so
        // geometry(scale) = chunk_size * scale = bake_px → fills the FBO exactly.
        let scale = bake_px as f64 / chunk_size as f64;

        let buf_size = Size::<i32, Buffer>::from((bake_px, bake_px));
        let mut tex =
            match Offscreen::<GlesTexture>::create_buffer(renderer, Fourcc::Abgr8888, buf_size) {
                Ok(t) => t,
                Err(e) => {
                    tracing::warn!("shader-bake (LOD {lod}, {cx},{cy}) create_buffer: {e}");
                    return;
                }
            };

        let area = Rectangle::<i32, Logical>::from_size((chunk_size, chunk_size).into());
        // u_time/u_zoom are pushed because the program declares all of
        // BG_UNIFORMS; eligible shaders don't read them (the eligibility gate
        // rejects u_time/u_zoom shaders), so the constants are inert.
        let elem = PixelShaderElement::new(
            self.shader.clone(),
            area,
            Some(vec![area]),
            1.0,
            vec![
                Uniform::new("u_camera", (origin.x as f32, origin.y as f32)),
                Uniform::new("u_time", 0.0f32),
                Uniform::new("u_zoom", 1.0f32),
            ],
            Kind::Unspecified,
        );

        let ok = {
            let mut target = match renderer.bind(&mut tex) {
                Ok(t) => t,
                Err(e) => {
                    tracing::warn!("shader-bake (LOD {lod}, {cx},{cy}) bind: {e}");
                    return;
                }
            };
            let mut dt = OutputDamageTracker::new(
                Size::<i32, Physical>::from((bake_px, bake_px)),
                Scale::from(scale),
                Transform::Normal,
            );
            let elements = [elem];
            dt.render_output(renderer, &mut target, 0, &elements, [0.0, 0.0, 0.0, 1.0])
                .is_ok()
        };
        if !ok {
            tracing::warn!("shader-bake (LOD {lod}, {cx},{cy}) render_output failed");
            return;
        }

        let bytes = (bake_px as u64) * (bake_px as u64) * 4;
        self.chunks.insert((lod, cx, cy), tex);
        self.chunk_meta.insert(
            (lod, cx, cy),
            ChunkMeta {
                bytes,
                last_touched_frame: frame,
            },
        );
        self.vram_bytes = self.vram_bytes.saturating_add(bytes);
    }

    fn evict_over_budget(&mut self) {
        if self.vram_bytes <= self.vram_budget_bytes {
            return;
        }
        let evicted = evict_lru_to_budget(
            &mut self.chunk_meta,
            &mut self.vram_bytes,
            self.vram_budget_bytes,
        );
        if evicted.is_empty() {
            return;
        }
        let evicted_set: HashSet<(u32, i32, i32)> = evicted.iter().copied().collect();
        for key in &evicted_set {
            self.chunks.remove(key);
        }
        self.chunk_elements
            .retain(|key, _| !evicted_set.contains(key));
        tracing::debug!(
            "shader-bake: evicted {} chunk(s), vram_bytes now {} / {}",
            evicted.len(),
            self.vram_bytes,
            self.vram_budget_bytes
        );
    }

    /// Per-frame entry point. Bakes the resident coarsest-LOD cover (baked once,
    /// reused across pans) and a budgeted slice of sharp target-LOD chunks, then
    /// emits one `PixelSnapRescaleElement` per visible cell — the finest cached
    /// chunk covering it. Never blank: the coarsest cover always covers the
    /// viewport. Evicts over budget last, so chunks chosen for display this
    /// frame survive.
    pub fn render_elements(
        &mut self,
        viewport: Rectangle<i32, Logical>,
        renderer: &mut GlesRenderer,
        camera: Point<f64, Logical>,
        zoom: f64,
    ) -> Vec<PixelSnapRescaleElement<TileShaderElement>> {
        #[cfg(feature = "profile-with-tracy")]
        let _span = tracy_client::span!("ShaderChunkCache::render_elements");

        self.frame_counter = self.frame_counter.wrapping_add(1);
        let frame = self.frame_counter;

        let target_lod = pick_lod(zoom, self.n_lods());
        let coarsest = self.n_lods() - 1;

        #[cfg(feature = "profile-with-tracy")]
        {
            static VRAM_PLOT: std::sync::OnceLock<tracy_client::PlotName> =
                std::sync::OnceLock::new();
            static TARGET_LOD_PLOT: std::sync::OnceLock<tracy_client::PlotName> =
                std::sync::OnceLock::new();
            let vram = VRAM_PLOT.get_or_init(|| {
                tracy_client::PlotName::new_leak("shader_chunks.vram_mb".to_string())
            });
            let target = TARGET_LOD_PLOT.get_or_init(|| {
                tracy_client::PlotName::new_leak("shader_chunks.target_lod".to_string())
            });
            if let Some(client) = tracy_client::Client::running() {
                client.plot(*vram, (self.vram_bytes as f64) / (1024.0 * 1024.0));
                client.plot(*target, target_lod as f64);
            }
        }

        // Resident coarse cover: the coarsest LOD spans a huge canvas area per
        // chunk, so the few cover cells are baked once and reused across pans —
        // the never-blank floor under the sharp layer. Stamp each frame so the
        // LRU keeps them resident.
        let coarsest_size = self.chunk_canvas_size_at(coarsest);
        let cover = visible_chunks(viewport, coarsest_size);
        for (cx, cy) in &cover {
            let key = (coarsest, *cx, *cy);
            if let Some(m) = self.chunk_meta.get_mut(&key) {
                m.last_touched_frame = frame;
            } else {
                self.bake_chunk(renderer, coarsest, *cx, *cy, frame);
            }
        }

        let target_size = self.chunk_canvas_size_at(target_lod);
        let visible_target = visible_chunks(viewport, target_size);

        // Graceful degrade: if cover + sharp working set can't both fit the
        // budget, baking sharp chunks would just evict still-visible ones every
        // frame (thrash + a busy-loop via `pending`). Skip fine refine and show
        // the coarse cover — blurrier, but stable. Each chunk is bake_px² * 4.
        let bytes_per_chunk = (self.bake_px as u64).pow(2) * 4;
        let working_set = (cover.len() + visible_target.len()) as u64 * bytes_per_chunk;
        let degraded = working_set > self.vram_budget_bytes;
        if degraded != self.degraded {
            self.degraded = degraded;
            if degraded {
                tracing::info!(
                    "shader-bake: cache_budget_mb too low for this view ({} MB working set > {} MB budget); \
                     showing coarse cover only. Raise cache_budget_mb for sharp detail.",
                    working_set / (1024 * 1024),
                    self.vram_budget_bytes / (1024 * 1024),
                );
            }
        }

        // Budgeted fine refine at the target LOD. Unbaked cells show the coarse
        // cover (or a finer intermediate from a prior zoom) until a later frame
        // sharpens them; `pending` keeps the udev loop firing meanwhile. Skipped
        // when target == coarsest (deep zoom-out) or degraded.
        let mut baked = 0usize;
        self.pending = false;
        if target_lod != coarsest && !degraded {
            for (cx, cy) in &visible_target {
                let key = (target_lod, *cx, *cy);
                if let Some(m) = self.chunk_meta.get_mut(&key) {
                    m.last_touched_frame = frame;
                    continue;
                }
                if baked < BAKE_BUDGET {
                    self.bake_chunk(renderer, target_lod, *cx, *cy, frame);
                    baked += 1;
                } else {
                    self.pending = true;
                }
            }
        }

        // Resolve coarse-to-fine: for each visible target cell, display the
        // finest cached chunk covering it. The coarsest is always present, so
        // this never comes up empty. Stamp the chosen chunk so an intermediate
        // cover still on screen survives eviction.
        let mut to_render: HashSet<(u32, i32, i32)> = HashSet::with_capacity(visible_target.len());
        for (cx, cy) in &visible_target {
            let canvas_x = cx * target_size;
            let canvas_y = cy * target_size;
            for lod in target_lod..=coarsest {
                let size = self.chunk_canvas_size_at(lod);
                let lx = canvas_x.div_euclid(size);
                let ly = canvas_y.div_euclid(size);
                let key = (lod, lx, ly);
                if self.chunks.contains_key(&key) {
                    if let Some(m) = self.chunk_meta.get_mut(&key) {
                        m.last_touched_frame = frame;
                    }
                    to_render.insert(key);
                    break;
                }
            }
        }

        self.chunk_elements.retain(|key, _| to_render.contains(key));

        let camera_i =
            Point::<i32, Logical>::from((camera.x.round() as i32, camera.y.round() as i32));

        // Deterministic order: smaller LOD (sharp) first = topmost in smithay's
        // z-order, so the sharp target layer draws over the coarse cover.
        let mut ordered: Vec<_> = to_render.into_iter().collect();
        ordered.sort_unstable();

        let mut out = Vec::with_capacity(ordered.len());
        for key in ordered {
            let (lod, cx, cy) = key;
            let chunk_size = self.chunk_canvas_size_at(lod);
            let origin = chunk_origin(cx, cy, chunk_size);
            let area = Rectangle::new(
                Point::from((origin.x - camera_i.x, origin.y - camera_i.y)),
                Size::from((chunk_size, chunk_size)),
            );
            let opaque = vec![area];

            let tex = self.chunks.get(&key).unwrap().clone();
            let tex_w = tex.width() as i32;
            let tex_h = tex.height() as i32;
            let shader = self.chunk_bg_shader.clone();
            let elem = self.chunk_elements.entry(key).or_insert_with(|| {
                TileShaderElement::new(
                    shader,
                    tex,
                    tex_w,
                    tex_h,
                    area,
                    Some(opaque.clone()),
                    1.0,
                    vec![],
                    Kind::Unspecified,
                )
            });
            // Idempotent when `area` is unchanged — no commit bump, no damage.
            elem.resize(area, Some(opaque));
            out.push(PixelSnapRescaleElement::from_element(
                elem.clone(),
                Point::<i32, Physical>::from((0, 0)),
                zoom,
            ));
        }

        // Evict last so chunks chosen for display this frame (stamped above) are
        // protected. Their textures are already cloned into `out`, so even if a
        // chunk were dropped from the map the current frame still renders.
        self.evict_over_budget();
        out
    }
}

/// Top-left canvas position of chunk `(cx, cy)` at a given chunk span.
pub(crate) fn chunk_origin(cx: i32, cy: i32, chunk_canvas_size: i32) -> Point<i32, Logical> {
    Point::from((cx * chunk_canvas_size, cy * chunk_canvas_size))
}

/// Chunk indices `(cx, cy)` whose canvas rect intersects `viewport`. Infinite
/// grid — no image bounds, no wrap. `div_euclid` handles negative canvas coords.
pub(crate) fn visible_chunks(
    viewport: Rectangle<i32, Logical>,
    chunk_canvas_size: i32,
) -> Vec<(i32, i32)> {
    if chunk_canvas_size <= 0 || viewport.size.w <= 0 || viewport.size.h <= 0 {
        return Vec::new();
    }
    let cx_min = viewport.loc.x.div_euclid(chunk_canvas_size);
    let cx_max = (viewport.loc.x + viewport.size.w - 1).div_euclid(chunk_canvas_size);
    let cy_min = viewport.loc.y.div_euclid(chunk_canvas_size);
    let cy_max = (viewport.loc.y + viewport.size.h - 1).div_euclid(chunk_canvas_size);
    let mut out =
        Vec::with_capacity(((cx_max - cx_min + 1) * (cy_max - cy_min + 1)).max(0) as usize);
    for cy in cy_min..=cy_max {
        for cx in cx_min..=cx_max {
            out.push((cx, cy));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: i32, y: i32, w: i32, h: i32) -> Rectangle<i32, Logical> {
        Rectangle::new(Point::from((x, y)), (w, h).into())
    }

    #[test]
    fn chunk_origin_basic() {
        assert_eq!(chunk_origin(0, 0, 1024), Point::from((0, 0)));
        assert_eq!(chunk_origin(1, 0, 1024), Point::from((1024, 0)));
        assert_eq!(chunk_origin(0, 1, 1024), Point::from((0, 1024)));
        assert_eq!(chunk_origin(-1, -2, 1024), Point::from((-1024, -2048)));
    }

    #[test]
    fn visible_single_chunk() {
        assert_eq!(visible_chunks(rect(100, 100, 100, 100), 1024), vec![(0, 0)]);
    }

    #[test]
    fn visible_spans_horizontal_pair() {
        assert_eq!(
            visible_chunks(rect(1000, 100, 100, 100), 1024),
            vec![(0, 0), (1, 0)]
        );
    }

    #[test]
    fn visible_spans_2x2() {
        assert_eq!(
            visible_chunks(rect(1000, 1000, 100, 100), 1024),
            vec![(0, 0), (1, 0), (0, 1), (1, 1)]
        );
    }

    #[test]
    fn visible_negative_canvas() {
        // Viewport straddling the origin: covers chunks (-1,-1)..(0,0).
        assert_eq!(
            visible_chunks(rect(-100, -100, 200, 200), 1024),
            vec![(-1, -1), (0, -1), (-1, 0), (0, 0)]
        );
    }

    #[test]
    fn visible_exact_boundary() {
        // Viewport exactly one chunk wide starting at a boundary → one chunk.
        assert_eq!(visible_chunks(rect(1024, 0, 1024, 1), 1024), vec![(1, 0)]);
    }

    #[test]
    fn visible_empty_viewport() {
        assert!(visible_chunks(rect(0, 0, 0, 100), 1024).is_empty());
        assert!(visible_chunks(rect(0, 0, 100, 0), 1024).is_empty());
    }

    #[test]
    fn visible_zero_chunk_size() {
        assert!(visible_chunks(rect(0, 0, 100, 100), 0).is_empty());
    }
}
