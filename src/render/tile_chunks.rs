//! Chunked tile-background rendering. `BgChunkCache` owns a [`TiffSource`] and
//! decodes+uploads tiles on demand at a zoom-selected LOD. When the requested
//! fine LOD isn't cached, [`chunk_render_elements`] falls back to the coarsest
//! cached LOD that covers the region (blurry-then-sharp loading).

use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::path::PathBuf;

use calloop::LoopSignal;
use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::{ImportMem, Texture};
use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::gles::{
    GlesRenderer, GlesTexProgram, GlesTexture, Uniform,
};
use smithay::utils::{Buffer, Logical, Physical, Point, Rectangle, Size};

use super::tile_chunks_tiff::TiffSource;
use super::tile_worker::{TileRequest, WorkerPool};
use super::{PixelSnapRescaleElement, TileShaderElement};

/// Hardcoded VRAM ceiling for cached chunked-bg tiles. Matches the `image`
/// crate's default decompression-bomb cap — same order of magnitude as the
/// memory a "reasonable" wallpaper consumes — and lets ~2000 256×256 RGBA8
/// tiles coexist, easily covering visible+nearby at any zoom.
const VRAM_BUDGET_BYTES: u64 = 512 * 1024 * 1024;

/// Cap on the fallback texture's longest side. Picks the finest LOD whose
/// `max(image_w, image_h) <= FALLBACK_MAX_DIM` as the constant-cost shader
/// fallback plane; everything finer is per-tile. This also sets the floor on
/// per-tile work: the coarsest per-tile LOD (just finer than the fallback)
/// carries the most visible elements per viewport, so it's the frame-time hot
/// spot (Tracy: ~15 ms / 42% over budget there). 4096 demotes that band into
/// the O(1) shader plane — same on-screen detail, no per-element cliff — at the
/// cost of a larger fallback texture (worst case ~67 MB at 4096², negligible vs
/// the 512 MB VRAM budget). A 16K source's coarsest page is still well below
/// 4096, so deep pyramids don't regress to a blurry fallback.
const FALLBACK_MAX_DIM: u32 = 4096;

/// Target render-chunk size in canvas units (= LOD-0 image pixels). Per-LOD
/// aggregation pads each chunk to roughly this size so visible-chunk count
/// stays ≈ 6 at every LOD's native zoom. Frame time at fractional zoom is
/// dominated by `PixelSnapRescaleElement` per-element rescale (~0.7 ms each
/// on M1); 4-6 elements ≈ shader-fallback frame time, so the LOD boundary
/// stops feeling like a perf cliff. Larger targets multiply per-chunk decode
/// work and VRAM 4×; smaller ones bring back wobble at fractional zoom.
const TARGET_CHUNK_CANVAS: i32 = 2048;

/// Smaller target for LOD 0. At zoom ≥ 1.0 the rescale is identity (low
/// per-element GPU cost), so the 2048 target that fights fractional-zoom
/// rescale cost elsewhere doesn't apply — but the un-aggregated 256-px tile
/// grid still emits ~60 elements per viewport and floods the decode queue
/// (Tracy: in-flight peaks ~140 vs ~10 elsewhere). 1024 cuts that to ~12
/// elements / 16 tiles per chunk while keeping sharpening granular enough that
/// freshly-revealed edges don't dwell on the magnified fallback.
const LOD0_TARGET_CHUNK_CANVAS: i32 = 1024;

#[derive(Debug, Clone, Copy)]
struct ChunkMeta {
    bytes: u64,
    last_touched_frame: u64,
}

pub struct BgChunkCache {
    /// LOD 0 image dimensions in canvas units (defines the wrap period and
    /// the clip rect against which every LOD's chunks are intersected).
    pub image_dims: (u32, u32),
    /// Canvas-coord position of the image's top-left corner. Set so the image
    /// is *centered* on canvas (0, 0) — i.e. `(-image_w/2, -image_h/2)`. Wrap
    /// seams then sit at ±image_w/2, ±image_h/2 rather than at the origin.
    pub image_position: Point<i32, Logical>,
    pub chunk_bg_shader: GlesTexProgram,
    /// Per-tile cache for LODs 0..coarsest-1. The coarsest LOD itself is
    /// served by [`fallback_texture`], so per-tile caching at that level would
    /// just duplicate VRAM.
    pub chunks: HashMap<(u32, i32, i32), GlesTexture>,
    pub chunk_elements: HashMap<(u32, i32, i32, i32, i32), TileShaderElement>,
    /// Per-LOD chunk canvas span (canvas units per render chunk at that LOD).
    /// Adjacent ratios are integer powers of 2 — the coarser-LOD fallback's
    /// `canvas_x.div_euclid(try_size)` lookup needs an integer multiple, which
    /// the aggregation pass guarantees.
    chunk_canvas_sizes: Vec<i32>,
    /// Per-LOD TIFF-tile aggregation factor (tiles per chunk edge). See
    /// `TARGET_CHUNK_CANVAS`.
    aggregations: Vec<u32>,
    chunk_meta: HashMap<(u32, i32, i32), ChunkMeta>,
    vram_bytes: u64,
    frame_counter: u64,
    /// Off-thread decoder pool. Render thread only ever calls
    /// `import_memory` on already-decoded blobs from the response channel —
    /// no libtiff work happens here. Pool shuts down cleanly on drop.
    pool: WorkerPool,
    /// Outstanding decode requests, keyed by `(lod, cx, cy)`. Used to dedupe
    /// (don't enqueue the same tile twice while one's already in flight) and
    /// to drive [`Self::has_pending_loads`].
    in_flight: HashSet<(u32, i32, i32)>,
    /// Tiles whose decode failed at least once. Skipped from future enqueues
    /// so a permanent file-format error doesn't spin the loop forever — the
    /// fallback plane covers that region instead.
    failed: HashSet<(u32, i32, i32)>,
    /// Whole coarsest-LOD image as one texture, eagerly decoded at init.
    /// Renders as the base plane under every per-tile fine chunk — gives
    /// every zoom level a guaranteed non-blank starting point.
    fallback_texture: GlesTexture,
    /// Reuses `tile_bg.glsl`: a single quad covers the visible viewport and
    /// the shader handles infinite wrap via modulo sampling — extreme
    /// zoom-out (100+ wrap instances visible) stays at constant cost.
    fallback_shader: GlesTexProgram,
    fallback_element: Option<TileShaderElement>,
    /// Last `(camera, canvas_w, canvas_h)` pushed to `fallback_element`. Skips
    /// the per-frame `update_uniforms` — and its full-viewport commit bump —
    /// when the viewport is unchanged; otherwise a static camera repaints the
    /// whole background (blur included) every frame.
    fallback_uniform_state: Option<(Point<f64, Logical>, i32, i32)>,
}

impl BgChunkCache {
    /// Caller must run [`ensure_visible_loaded`](Self::ensure_visible_loaded)
    /// before [`chunk_render_elements`] each frame so per-tile fine LODs get a
    /// chance to upload — the fallback plane always renders, but without
    /// `ensure_visible_loaded` it's the only thing on screen.
    ///
    /// Spawns a decoder thread pool ([`WorkerPool`]) which reopens the TIFF
    /// once per worker (libtiff isn't safe to share). The init-scan
    /// `TiffSource` is used only for the fallback-plane decode and is
    /// dropped immediately after.
    pub fn new_from_tiff(
        mut source: TiffSource,
        path: PathBuf,
        chunk_bg_shader: GlesTexProgram,
        fallback_shader: GlesTexProgram,
        renderer: &mut GlesRenderer,
        loop_signal: LoopSignal,
    ) -> Result<Self, String> {
        for (i, meta) in source.lods().iter().enumerate() {
            if meta.tile_dims.0 != meta.tile_dims.1 {
                return Err(format!(
                    "LOD {i}: non-square tile dims {:?} not supported",
                    meta.tile_dims
                ));
            }
        }
        let lod0 = *source
            .lods()
            .first()
            .ok_or("TIFF has no LOD levels")?;
        let image_dims = lod0.image_dims;
        let n_on_disk = source.lods().len();
        let (mut chunk_canvas_sizes, mut aggregations) = derive_chunk_layout(&source)?;
        let n_after_vips_trunc = chunk_canvas_sizes.len();

        // Pick fallback LOD: the FINEST (lowest-index) LOD whose dimensions
        // fit within `FALLBACK_MAX_DIM`. Deep pyramids would otherwise pin
        // the fallback to a too-blurry coarsest page.
        let mut fallback_lod = (n_after_vips_trunc - 1) as u32;
        for i in 0..n_after_vips_trunc {
            let dims = source.lods()[i].image_dims;
            if dims.0.max(dims.1) <= FALLBACK_MAX_DIM {
                fallback_lod = i as u32;
                break;
            }
        }
        // LODs past the fallback are unused (per-tile loading stops at
        // `fallback_lod - 1`; pick_lod clamps to `fallback_lod`). Truncate so
        // `n_lods()` reports the actually-used count.
        chunk_canvas_sizes.truncate((fallback_lod + 1) as usize);
        aggregations.truncate((fallback_lod + 1) as usize);
        debug_assert_eq!(chunk_canvas_sizes.len(), aggregations.len());
        // `n_lods() >= 1` invariant relied on by `coarsest = n_lods - 1` in
        // the per-frame paths, which subtract without a zero-guard.
        debug_assert!(!chunk_canvas_sizes.is_empty(), "at least one usable LOD");
        tracing::info!(
            "chunked tile-bg: TIFF has {n_on_disk} pages on disk; {} pass vips ±1 truncation; \
             using LODs 0-{fallback_lod} (LOD {fallback_lod} as fallback plane, {}×{} px); \
             chunk_canvas_sizes={chunk_canvas_sizes:?}, aggregations={aggregations:?}",
            n_after_vips_trunc,
            source.lods()[fallback_lod as usize].image_dims.0,
            source.lods()[fallback_lod as usize].image_dims.1,
        );

        let fallback = source.read_whole_lod(fallback_lod)?;
        let fallback_texture = renderer
            .import_memory(
                &fallback.rgba,
                Fourcc::Abgr8888,
                Size::<i32, Buffer>::from((fallback.width as i32, fallback.height as i32)),
                false,
            )
            .map_err(|e| format!("fallback texture upload: {e}"))?;

        drop(source);
        let pool = WorkerPool::spawn(path, loop_signal)?;

        let image_position = Point::from((
            -(image_dims.0 as i32) / 2,
            -(image_dims.1 as i32) / 2,
        ));
        Ok(Self {
            image_dims,
            image_position,
            chunk_bg_shader,
            chunks: HashMap::new(),
            chunk_elements: HashMap::new(),
            chunk_canvas_sizes,
            aggregations,
            chunk_meta: HashMap::new(),
            vram_bytes: 0,
            frame_counter: 0,
            pool,
            in_flight: HashSet::new(),
            failed: HashSet::new(),
            fallback_texture,
            fallback_shader,
            fallback_element: None,
            fallback_uniform_state: None,
        })
    }

    pub fn n_lods(&self) -> u32 {
        self.chunk_canvas_sizes.len() as u32
    }

    pub fn chunk_canvas_size_at(&self, lod: u32) -> i32 {
        self.chunk_canvas_sizes[lod as usize]
    }

    pub fn aggregation_at(&self, lod: u32) -> u32 {
        self.aggregations[lod as usize]
    }

    /// Per-frame entry point: drain ready decodes onto the GPU, stamp LRU, then
    /// enqueue newly-needed visible tiles. At most `upload_budget` blobs upload
    /// per frame, capping render-thread GPU import time when workers burst
    /// faster than the compositor can absorb. The worker queue is unbounded by
    /// design — a fast pan can enqueue 50+ requests in one frame and workers
    /// self-throttle via decode time.
    ///
    /// Returns the number of tiles uploaded this frame.
    pub fn ensure_visible_loaded(
        &mut self,
        viewport: Rectangle<i32, Logical>,
        renderer: &mut GlesRenderer,
        zoom: f64,
        upload_budget: usize,
    ) -> usize {
        #[cfg(feature = "profile-with-tracy")]
        let _span = tracy_client::span!("BgChunkCache::ensure_visible_loaded");

        self.frame_counter = self.frame_counter.wrapping_add(1);
        let frame = self.frame_counter;

        let uploaded = self.drain_responses(renderer, upload_budget, frame);

        let target_lod = pick_lod(zoom, self.n_lods());
        let target_chunk_size = self.chunk_canvas_size_at(target_lod);
        let visible = visibility_query(
            viewport,
            self.image_position,
            self.image_dims,
            target_chunk_size,
        );
        let mut wanted: HashSet<(i32, i32)> = HashSet::with_capacity(visible.len());
        for (cx, cy, _kx, _ky) in &visible {
            wanted.insert((*cx, *cy));
        }

        // Stamp coarser-LOD fallback ancestors so eviction can't drop the
        // coarse cover while the fine LOD is still loading — would flash blank.
        // Bound is `..coarsest` (exclusive): the coarsest LOD is the always-
        // resident fallback plane, never inserted into chunk_meta, so a
        // `get_mut` lookup there is wasted work.
        let n_lods = self.n_lods();
        let coarsest = n_lods - 1;
        for (cx_t, cy_t) in &wanted {
            let canvas_x = cx_t * target_chunk_size;
            let canvas_y = cy_t * target_chunk_size;
            for try_lod in target_lod..coarsest {
                let try_size = self.chunk_canvas_size_at(try_lod);
                let try_cx = canvas_x.div_euclid(try_size);
                let try_cy = canvas_y.div_euclid(try_size);
                if let Some(m) = self.chunk_meta.get_mut(&(try_lod, try_cx, try_cy)) {
                    m.last_touched_frame = frame;
                }
            }
        }

        // Enqueue tiles from every LOD strictly finer than the fallback,
        // coarse-first — coarse LODs cover a wide area in 1-4 tiles each, so
        // landing a coarse LOD first gives every fine chunk a sharper-than-
        // fallback overlay while it waits its turn.
        let mut enqueued = 0usize;
        if target_lod < coarsest {
            for lod in (target_lod..coarsest).rev() {
                let size = self.chunk_canvas_size_at(lod);
                let visible = visibility_query(
                    viewport,
                    self.image_position,
                    self.image_dims,
                    size,
                );
                let mut at_lod: HashSet<(i32, i32)> = HashSet::with_capacity(visible.len());
                for (cx, cy, _kx, _ky) in &visible {
                    at_lod.insert((*cx, *cy));
                }
                let aggregation = self.aggregation_at(lod);
                for (cx, cy) in at_lod {
                    let key = (lod, cx, cy);
                    if self.chunks.contains_key(&key)
                        || self.in_flight.contains(&key)
                        || self.failed.contains(&key)
                    {
                        continue;
                    }
                    let Ok(cx_u) = u32::try_from(cx) else { continue };
                    let Ok(cy_u) = u32::try_from(cy) else { continue };
                    self.in_flight.insert(key);
                    self.pool.enqueue(TileRequest {
                        lod,
                        cx: cx_u,
                        cy: cy_u,
                        aggregation,
                    });
                    enqueued += 1;
                }
            }
        }
        if enqueued > 0 {
            tracing::trace!(
                "chunked tile-bg: enqueued {enqueued} tile(s); {} in-flight",
                self.in_flight.len()
            );
        }
        self.evict_over_budget();
        uploaded
    }

    /// True while any decode is still in flight. The udev render scheduler reads
    /// this to keep firing frames until the visible set resolves — otherwise
    /// missing fine chunks stay covered by the fallback plane until external
    /// damage (cursor, animation, client commit) re-wakes the loop. Failed
    /// decodes land in `self.failed` and are never re-enqueued, so they don't
    /// keep this true.
    pub fn has_pending_loads(&self) -> bool {
        !self.in_flight.is_empty()
    }

    fn drain_responses(
        &mut self,
        renderer: &mut GlesRenderer,
        upload_budget: usize,
        frame: u64,
    ) -> usize {
        #[cfg(feature = "profile-with-tracy")]
        let _span = tracy_client::span!("BgChunkCache::drain_responses");

        let mut uploaded = 0;
        while uploaded < upload_budget {
            let Some(resp) = self.pool.try_recv() else {
                break;
            };
            let key = (resp.req.lod, resp.req.cx as i32, resp.req.cy as i32);
            self.in_flight.remove(&key);
            match resp.result {
                Ok(tile) => {
                    match renderer.import_memory(
                        &tile.rgba,
                        Fourcc::Abgr8888,
                        Size::<i32, Buffer>::from((tile.width as i32, tile.height as i32)),
                        false,
                    ) {
                        Ok(tex) => {
                            let bytes = (tex.width() as u64) * (tex.height() as u64) * 4;
                            self.chunks.insert(key, tex);
                            self.vram_bytes = self.vram_bytes.saturating_add(bytes);
                            self.chunk_meta.insert(
                                key,
                                ChunkMeta {
                                    bytes,
                                    last_touched_frame: frame,
                                },
                            );
                            uploaded += 1;
                        }
                        Err(e) => {
                            tracing::warn!(
                                "chunked tile (LOD {}, {},{}) import_memory: {e}",
                                resp.req.lod, resp.req.cx, resp.req.cy
                            );
                            self.failed.insert(key);
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        "chunked tile (LOD {}, {},{}) decode: {e}",
                        resp.req.lod, resp.req.cx, resp.req.cy
                    );
                    self.failed.insert(key);
                }
            }
        }
        uploaded
    }

    fn evict_over_budget(&mut self) {
        if self.vram_bytes <= VRAM_BUDGET_BYTES {
            return;
        }
        let evicted = evict_lru_to_budget(
            &mut self.chunk_meta,
            &mut self.vram_bytes,
            VRAM_BUDGET_BYTES,
        );
        if evicted.is_empty() {
            return;
        }
        // One O(N+M) retain pass beats N-per-key removal.
        let evicted_set: HashSet<(u32, i32, i32)> = evicted.iter().copied().collect();
        for key in &evicted_set {
            self.chunks.remove(key);
        }
        self.chunk_elements.retain(|elem_key, _| {
            !evicted_set.contains(&(elem_key.0, elem_key.1, elem_key.2))
        });
        tracing::debug!(
            "chunked tile-bg: evicted {} tile(s), vram_bytes now {} / {}",
            evicted.len(),
            self.vram_bytes,
            VRAM_BUDGET_BYTES
        );
    }

}

/// Returns evicted keys; caller drops them from the parallel texture map.
/// Split out as a pure function so the LRU math is unit-testable without GLES.
fn evict_lru_to_budget(
    meta: &mut HashMap<(u32, i32, i32), ChunkMeta>,
    vram_bytes: &mut u64,
    budget: u64,
) -> Vec<(u32, i32, i32)> {
    let mut evicted = Vec::new();
    if *vram_bytes <= budget {
        return evicted;
    }
    let mut by_age: Vec<((u32, i32, i32), u64)> = meta
        .iter()
        .map(|(k, m)| (*k, m.last_touched_frame))
        .collect();
    by_age.sort_unstable_by_key(|(_, t)| *t);
    for (key, _) in by_age {
        if *vram_bytes <= budget {
            break;
        }
        if let Some(m) = meta.remove(&key) {
            *vram_bytes = vram_bytes.saturating_sub(m.bytes);
            evicted.push(key);
        }
    }
    evicted
}

/// Pick the LOD whose sample density meets the screen's needs at this zoom.
/// Clamped to `[0, n_lods - 1]`. At `zoom >= 1.0` always returns LOD 0.
/// NaN/non-finite zoom returns LOD 0 (defensive — animation math shouldn't
/// produce NaN, but a clamp prevents a panic from `as u32` on a NaN cast).
///
/// Hybrid policy: `ceil(-log2(zoom))` for inter-LOD transitions (snappy detail
/// downgrades, no per-tile inefficiency at zoom boundaries), but the boundary
/// INTO the LAST LOD lags by a full octave (`raw < last - 1.0`) rather than the
/// half-octave geometric mean. The last LOD is the shader fallback plane;
/// entering it loses per-tile sharpness, but staying *out* through the coarsest
/// per-tile band costs perf — that band holds the most visible tiles per
/// viewport (30–70 elements at LOD N-1 on a 16K image). A full octave puts the
/// transition at a power-of-two zoom, where the visual mismatch is least
/// jarring.
pub(crate) fn pick_lod(zoom: f64, n_lods: u32) -> u32 {
    if n_lods == 0 {
        return 0;
    }
    let last = n_lods - 1;
    if zoom >= 1.0 || !zoom.is_finite() {
        return 0;
    }
    if zoom <= 0.0 {
        return last;
    }
    let raw = -zoom.log2();
    if !raw.is_finite() || raw < 0.0 {
        return 0;
    }
    let target_ceil = raw.ceil() as u32;
    // If ceil would pick the fallback LOD but we're not yet past the
    // power-of-two boundary (`raw >= last - 1.0`), stay one step finer.
    if target_ceil >= last && raw < last as f64 - 1.0 {
        last.saturating_sub(1)
    } else {
        target_ceil.min(last)
    }
}

/// Compute per-LOD canvas chunk sizes and TIFF-tile aggregation factors.
/// Truncates the pyramid at the first non-2× *raw* transition: vips's
/// `--pyramid` produces power-of-2 chains but the last page can drift ±1 px
/// when source dims don't divide evenly, and the fallback's `div_euclid`
/// lookup can't tolerate that straddle.
fn derive_chunk_layout(source: &TiffSource) -> Result<(Vec<i32>, Vec<u32>), String> {
    let lods = source.lods();
    let lod_0_w = lods[0].image_dims.0 as f64;
    let mut raw = Vec::with_capacity(lods.len());
    for (i, meta) in lods.iter().enumerate() {
        let scale = lod_0_w / meta.image_dims.0 as f64;
        let s = (meta.tile_dims.0 as f64 * scale).round() as i32;
        if s <= 0 {
            return Err(format!("LOD {i}: derived canvas chunk size {s} <= 0"));
        }
        raw.push(s);
    }
    for i in 1..raw.len() {
        if raw[i] != 2 * raw[i - 1] {
            tracing::warn!(
                "chunked tile-bg: LOD {i} raw canvas chunk size {} != 2× LOD {} ({}); \
                 truncating pyramid to {} usable LOD(s)",
                raw[i],
                i - 1,
                raw[i - 1],
                i,
            );
            raw.truncate(i);
            break;
        }
    }
    let aggregations =
        compute_aggregations(&raw, TARGET_CHUNK_CANVAS, LOD0_TARGET_CHUNK_CANVAS);
    let sizes: Vec<i32> = raw
        .iter()
        .zip(&aggregations)
        .map(|(r, a)| r * (*a as i32))
        .collect();
    Ok((sizes, aggregations))
}

/// Per-LOD TIFF-tile aggregation factor. Each LOD floor-divides its target by
/// `raw_size` so the final chunk size never exceeds the target, then clamps to
/// ≥ 1 (guarding raw_size == 0). LOD 0 uses `lod0_target` (smaller — see
/// [`LOD0_TARGET_CHUNK_CANVAS`]); every coarser LOD uses `target`.
pub(crate) fn compute_aggregations(
    raw_sizes: &[i32],
    target: i32,
    lod0_target: i32,
) -> Vec<u32> {
    let mut agg = vec![1u32; raw_sizes.len()];
    for i in 0..raw_sizes.len() {
        let t = if i == 0 { lod0_target } else { target };
        if raw_sizes[i] > 0 && raw_sizes[i] < t {
            agg[i] = ((t / raw_sizes[i]) as u32).max(1);
        }
    }
    agg
}

/// Integer `k` offsets along one axis such that image instance `k` overlaps
/// the viewport span `[viewport_min, viewport_max)`. Returns an exclusive
/// range; iterate it to enumerate visible instances. Empty viewport → empty.
pub(crate) fn compute_k_range(
    viewport_min: i32,
    viewport_max: i32,
    image_origin: i32,
    image_size: i32,
) -> Range<i32> {
    if viewport_max <= viewport_min || image_size <= 0 {
        return 0..0;
    }
    // Half-open viewport [viewport_min, viewport_max). Instance `k` occupies
    // `[image_origin + k*size, image_origin + (k+1)*size)`. `div_euclid` rounds
    // toward -inf, so at the exact boundary `viewport_min = image_origin + k*size`
    // it returns `k` (the instance starting at the boundary), not `k-1`.
    let k_min = (viewport_min - image_origin).div_euclid(image_size);
    let k_max = (viewport_max - 1 - image_origin).div_euclid(image_size);
    k_min..(k_max + 1)
}

/// Chunk indices `(cx, cy)` of the image instance at `instance_origin` whose
/// canvas rect intersects `viewport`. Clips against image bounds — chunks
/// past the image edge are not emitted (the calling visibility query covers
/// past-edge regions with neighbor instances at different `k` offsets).
pub(crate) fn chunks_intersecting(
    viewport: Rectangle<i32, Logical>,
    instance_origin: Point<i32, Logical>,
    chunk_canvas_size: i32,
    image_dims: (u32, u32),
) -> Vec<(i32, i32)> {
    if chunk_canvas_size <= 0 {
        return Vec::new();
    }
    let img_w = image_dims.0 as i32;
    let img_h = image_dims.1 as i32;

    let rel_min_x = viewport.loc.x - instance_origin.x;
    let rel_min_y = viewport.loc.y - instance_origin.y;
    let rel_max_x = rel_min_x + viewport.size.w;
    let rel_max_y = rel_min_y + viewport.size.h;

    let clip_min_x = rel_min_x.max(0);
    let clip_min_y = rel_min_y.max(0);
    let clip_max_x = rel_max_x.min(img_w);
    let clip_max_y = rel_max_y.min(img_h);

    if clip_min_x >= clip_max_x || clip_min_y >= clip_max_y {
        return Vec::new();
    }

    let cx_min = clip_min_x / chunk_canvas_size;
    let cx_max = (clip_max_x - 1) / chunk_canvas_size;
    let cy_min = clip_min_y / chunk_canvas_size;
    let cy_max = (clip_max_y - 1) / chunk_canvas_size;

    let mut chunks =
        Vec::with_capacity(((cx_max - cx_min + 1) * (cy_max - cy_min + 1)) as usize);
    for cy in cy_min..=cy_max {
        for cx in cx_min..=cx_max {
            chunks.push((cx, cy));
        }
    }
    chunks
}

/// Top-left canvas position of chunk `(cx, cy)` of image instance `(kx, ky)`.
pub(crate) fn chunk_canvas_origin(
    cx: i32,
    cy: i32,
    kx: i32,
    ky: i32,
    image_position: Point<i32, Logical>,
    image_dims: (u32, u32),
    chunk_canvas_size: i32,
) -> Point<i32, Logical> {
    Point::from((
        image_position.x + kx * image_dims.0 as i32 + cx * chunk_canvas_size,
        image_position.y + ky * image_dims.1 as i32 + cy * chunk_canvas_size,
    ))
}

/// Per-frame visibility + element create/reuse. Emits cached per-tile fine
/// chunks first (drawn on top in smithay's z-order: first in vec = topmost),
/// then a fallback-plane element per wrap offset that always renders the
/// whole image at coarsest-LOD detail underneath. No chunk is ever blank.
///
/// * Inner `TileShaderElement` is persistent per element key with a stable
///   `Id`; `resize()` is idempotent so a static camera leaves the commit
///   counter alone and the damage tracker preserves the frame.
/// * All chunks share a `(0, 0)` rounding anchor in `PixelSnapRescaleElement`
///   so adjacent chunks meet at pixel-consistent edges at fractional zoom —
///   actual position lives in the inner element's `area`.
pub fn chunk_render_elements(
    cache: &mut BgChunkCache,
    viewport: Rectangle<i32, Logical>,
    camera: Point<f64, Logical>,
    zoom: f64,
) -> Vec<PixelSnapRescaleElement<TileShaderElement>> {
    #[cfg(feature = "profile-with-tracy")]
    let _span = tracy_client::span!("chunk_render_elements");

    let target_lod = pick_lod(zoom, cache.n_lods());
    let n_lods = cache.n_lods();
    let coarsest = n_lods - 1;

    #[cfg(feature = "profile-with-tracy")]
    {
        // Static plot names (`new_leak`) so each survives across frames as the
        // same line on the Tracy timeline.
        static VRAM_PLOT: std::sync::OnceLock<tracy_client::PlotName> =
            std::sync::OnceLock::new();
        static IN_FLIGHT_PLOT: std::sync::OnceLock<tracy_client::PlotName> =
            std::sync::OnceLock::new();
        static TARGET_LOD_PLOT: std::sync::OnceLock<tracy_client::PlotName> =
            std::sync::OnceLock::new();
        let vram = VRAM_PLOT.get_or_init(|| {
            tracy_client::PlotName::new_leak("bg_chunks.vram_mb".to_string())
        });
        let in_flight = IN_FLIGHT_PLOT.get_or_init(|| {
            tracy_client::PlotName::new_leak("bg_chunks.in_flight".to_string())
        });
        let target = TARGET_LOD_PLOT.get_or_init(|| {
            tracy_client::PlotName::new_leak("bg_chunks.target_lod".to_string())
        });
        if let Some(client) = tracy_client::Client::running() {
            client.plot(*vram, (cache.vram_bytes as f64) / (1024.0 * 1024.0));
            client.plot(*in_flight, cache.in_flight.len() as f64);
            client.plot(*target, target_lod as f64);
        }
    }

    // Per-tile resolve only across LODs strictly finer than coarsest — the
    // coarsest LOD is the fallback plane, no per-tile cache there.
    let mut to_render: HashSet<(u32, i32, i32, i32, i32)> = HashSet::new();
    if target_lod < coarsest {
        let target_chunk_size = cache.chunk_canvas_size_at(target_lod);
        let visible_target = visibility_query(
            viewport,
            cache.image_position,
            cache.image_dims,
            target_chunk_size,
        );
        to_render.reserve(visible_target.len());
        for (cx_t, cy_t, kx, ky) in &visible_target {
            // (kx, ky) factors into element placement, not LOD lookup — coarser
            // LODs are picked on the k=0-instance canvas origin.
            let target_cx_canvas = cx_t * target_chunk_size;
            let target_cy_canvas = cy_t * target_chunk_size;
            for try_lod in target_lod..coarsest {
                let try_size = cache.chunk_canvas_size_at(try_lod);
                let try_cx = target_cx_canvas.div_euclid(try_size);
                let try_cy = target_cy_canvas.div_euclid(try_size);
                if cache.chunks.contains_key(&(try_lod, try_cx, try_cy)) {
                    to_render.insert((try_lod, try_cx, try_cy, *kx, *ky));
                    break;
                }
            }
        }
    }

    cache
        .chunk_elements
        .retain(|key, _| to_render.contains(key));

    let camera_i = Point::<i32, Logical>::from((camera.x.round() as i32, camera.y.round() as i32));
    let image_dims = cache.image_dims;
    let image_position = cache.image_position;

    // Sort for deterministic emission order: HashSet iteration is non-
    // deterministic, and overlapping coarse tiles would otherwise z-fight.
    let mut ordered: Vec<_> = to_render.into_iter().collect();
    ordered.sort_unstable();

    let mut out = Vec::with_capacity(ordered.len() + 1);
    for key in ordered {
        let (lod, cx, cy, kx, ky) = key;
        let chunk_size = cache.chunk_canvas_size_at(lod);
        // Actual (possibly partial) size keeps right/bottom edge chunks from
        // stretching past the next wrap-offset instance. Texture pixel dims come
        // from the GPU texture (`tex.width()/height()`), not the canvas span —
        // at coarse LODs the texture is far smaller, so passing canvas units as
        // tex_w/tex_h would sample a tiny corner into a huge area.
        let (chunk_w, chunk_h) = chunk_actual_size(cx, cy, image_dims, chunk_size);
        let canvas_origin =
            chunk_canvas_origin(cx, cy, kx, ky, image_position, image_dims, chunk_size);
        let area = Rectangle::new(
            Point::from((canvas_origin.x - camera_i.x, canvas_origin.y - camera_i.y)),
            Size::from((chunk_w, chunk_h)),
        );
        let opaque = vec![area];

        // Resolve loop above only inserts keys whose `chunks` entry exists.
        let tex = cache.chunks.get(&(lod, cx, cy)).unwrap().clone();
        let tex_w = tex.width() as i32;
        let tex_h = tex.height() as i32;
        let elem = cache.chunk_elements.entry(key).or_insert_with(|| {
            TileShaderElement::new(
                cache.chunk_bg_shader.clone(),
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
        // `camera_i` is integer-rounded so sub-pixel camera drift (<= 0.5
        // logical px) produces the same `area` and the same no-damage path.
        elem.resize(area, Some(opaque));
        out.push(PixelSnapRescaleElement::from_element(
            elem.clone(),
            Point::<i32, Physical>::from((0, 0)),
            zoom,
        ));
    }

    // Single fallback element via `tile_bg.glsl` — the shader handles wrap
    // via modulo sampling, so the cost is constant regardless of how many
    // image instances the viewport spans.
    let canvas_w = viewport.size.w.max(1);
    let canvas_h = viewport.size.h.max(1);
    let fallback_area = Rectangle::new(
        Point::<i32, Logical>::from((
            viewport.loc.x - camera_i.x,
            viewport.loc.y - camera_i.y,
        )),
        Size::from((canvas_w, canvas_h)),
    );
    let fallback_opaque = vec![fallback_area];
    let fallback_uniforms = vec![
        Uniform::new("u_camera", (camera.x as f32, camera.y as f32)),
        Uniform::new(
            "u_tile_size",
            (image_dims.0 as f32, image_dims.1 as f32),
        ),
        Uniform::new("u_output_size", (canvas_w as f32, canvas_h as f32)),
    ];
    let fallback_tex_w = cache.fallback_texture.width() as i32;
    let fallback_tex_h = cache.fallback_texture.height() as i32;
    // Only camera and output size feed the fallback uniforms (u_tile_size is
    // constant). Re-push — and take its full-viewport commit bump — only when
    // one changed; else a static-camera frame (cursor move, client commit, tile
    // upload) would needlessly repaint the whole bg.
    let needs_uniform_update =
        cache.fallback_uniform_state != Some((camera, canvas_w, canvas_h));
    let fb = cache.fallback_element.get_or_insert_with(|| {
        TileShaderElement::new(
            cache.fallback_shader.clone(),
            cache.fallback_texture.clone(),
            fallback_tex_w,
            fallback_tex_h,
            fallback_area,
            Some(fallback_opaque.clone()),
            1.0,
            fallback_uniforms.clone(),
            Kind::Unspecified,
        )
    });
    fb.resize(fallback_area, Some(fallback_opaque));
    if needs_uniform_update {
        fb.update_uniforms(fallback_uniforms);
    }
    out.push(PixelSnapRescaleElement::from_element(
        fb.clone(),
        Point::<i32, Physical>::from((0, 0)),
        zoom,
    ));
    cache.fallback_uniform_state = Some((camera, canvas_w, canvas_h));
    out
}

/// Pixel dimensions of chunk `(cx, cy)`; smaller at right/bottom edges.
pub(crate) fn chunk_actual_size(
    cx: i32,
    cy: i32,
    image_dims: (u32, u32),
    chunk_canvas_size: i32,
) -> (i32, i32) {
    let x_px = cx * chunk_canvas_size;
    let y_px = cy * chunk_canvas_size;
    let w = (image_dims.0 as i32 - x_px).clamp(0, chunk_canvas_size);
    let h = (image_dims.1 as i32 - y_px).clamp(0, chunk_canvas_size);
    (w, h)
}

/// All visible `(cx, cy, kx, ky)` keys for the given viewport.
pub(crate) fn visibility_query(
    viewport: Rectangle<i32, Logical>,
    image_position: Point<i32, Logical>,
    image_dims: (u32, u32),
    chunk_canvas_size: i32,
) -> Vec<(i32, i32, i32, i32)> {
    let img_w = image_dims.0 as i32;
    let img_h = image_dims.1 as i32;
    let kx_range = compute_k_range(
        viewport.loc.x,
        viewport.loc.x + viewport.size.w,
        image_position.x,
        img_w,
    );
    let ky_range = compute_k_range(
        viewport.loc.y,
        viewport.loc.y + viewport.size.h,
        image_position.y,
        img_h,
    );

    let mut out = Vec::new();
    for ky in ky_range {
        for kx in kx_range.clone() {
            let instance_origin = Point::from((
                image_position.x + kx * img_w,
                image_position.y + ky * img_h,
            ));
            for (cx, cy) in chunks_intersecting(viewport, instance_origin, chunk_canvas_size, image_dims) {
                out.push((cx, cy, kx, ky));
            }
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
    fn k_range_single_instance_at_origin() {
        assert_eq!(compute_k_range(0, 100, 0, 200), 0..1);
    }

    #[test]
    fn k_range_spans_two_instances() {
        assert_eq!(compute_k_range(150, 250, 0, 200), 0..2);
    }

    #[test]
    fn k_range_negative_offset() {
        assert_eq!(compute_k_range(-50, 50, 0, 200), -1..1);
    }

    #[test]
    fn k_range_with_image_origin() {
        assert_eq!(compute_k_range(50, 350, 100, 200), -1..2);
    }

    #[test]
    fn k_range_with_negative_image_origin() {
        // image_origin = -100 → instance k=0 spans [-100, 100). Viewport [-50, 50)
        // sits entirely in k=0. div_euclid handles the negative origin correctly.
        assert_eq!(compute_k_range(-50, 50, -100, 200), 0..1);
        // Viewport [-150, -50) overlaps k=-1 [-300, -100) and k=0 [-100, 100).
        assert_eq!(compute_k_range(-150, -50, -100, 200), -1..1);
    }

    #[test]
    fn k_range_empty_viewport() {
        assert_eq!(compute_k_range(100, 100, 0, 200), 0..0);
        assert_eq!(compute_k_range(100, 50, 0, 200), 0..0);
    }

    #[test]
    fn k_range_exact_boundary() {
        assert_eq!(compute_k_range(0, 200, 0, 200), 0..1);
        assert_eq!(compute_k_range(200, 400, 0, 200), 1..2);
    }

    #[test]
    fn chunks_intersecting_inside_single_chunk() {
        let chunks = chunks_intersecting(rect(100, 100, 100, 100), Point::from((0, 0)), 1024, (2048, 2048));
        assert_eq!(chunks, vec![(0, 0)]);
    }

    #[test]
    fn chunks_intersecting_spans_horizontal_pair() {
        let chunks = chunks_intersecting(rect(1000, 100, 100, 100), Point::from((0, 0)), 1024, (2048, 2048));
        assert_eq!(chunks, vec![(0, 0), (1, 0)]);
    }

    #[test]
    fn chunks_intersecting_spans_2x2() {
        let chunks = chunks_intersecting(rect(1000, 1000, 100, 100), Point::from((0, 0)), 1024, (2048, 2048));
        assert_eq!(chunks, vec![(0, 0), (1, 0), (0, 1), (1, 1)]);
    }

    #[test]
    fn chunks_intersecting_clips_past_image_edge() {
        let chunks = chunks_intersecting(rect(1500, 0, 1000, 100), Point::from((0, 0)), 1024, (2048, 2048));
        assert_eq!(chunks, vec![(1, 0)]);
    }

    #[test]
    fn chunks_intersecting_viewport_entirely_past_image() {
        let chunks = chunks_intersecting(rect(3000, 0, 100, 100), Point::from((0, 0)), 1024, (2048, 2048));
        assert!(chunks.is_empty());
    }

    #[test]
    fn chunks_intersecting_with_instance_origin() {
        let chunks = chunks_intersecting(rect(1100, 100, 100, 100), Point::from((1000, 0)), 1024, (2048, 2048));
        assert_eq!(chunks, vec![(0, 0)]);
    }

    #[test]
    fn chunks_intersecting_partial_edge_chunk() {
        let chunks = chunks_intersecting(rect(1200, 100, 100, 100), Point::from((0, 0)), 1024, (1500, 1500));
        assert_eq!(chunks, vec![(1, 0)]);
    }

    #[test]
    fn chunk_canvas_origin_basic() {
        assert_eq!(
            chunk_canvas_origin(0, 0, 0, 0, Point::from((0, 0)), (2048, 2048), 1024),
            Point::from((0, 0))
        );
        assert_eq!(
            chunk_canvas_origin(1, 0, 0, 0, Point::from((0, 0)), (2048, 2048), 1024),
            Point::from((1024, 0))
        );
        assert_eq!(
            chunk_canvas_origin(0, 1, 0, 0, Point::from((0, 0)), (2048, 2048), 1024),
            Point::from((0, 1024))
        );
    }

    #[test]
    fn chunk_canvas_origin_with_image_position() {
        assert_eq!(
            chunk_canvas_origin(1, 1, 0, 0, Point::from((500, 500)), (2048, 2048), 1024),
            Point::from((1524, 1524))
        );
    }

    #[test]
    fn chunk_canvas_origin_with_wrap_offset() {
        assert_eq!(
            chunk_canvas_origin(0, 0, 1, 0, Point::from((0, 0)), (2048, 2048), 1024),
            Point::from((2048, 0))
        );
        assert_eq!(
            chunk_canvas_origin(0, 0, -1, 0, Point::from((0, 0)), (2048, 2048), 1024),
            Point::from((-2048, 0))
        );
    }

    #[test]
    fn chunk_actual_size_interior_chunk_is_full() {
        assert_eq!(chunk_actual_size(0, 0, (2048, 2048), 1024), (1024, 1024));
    }

    #[test]
    fn chunk_actual_size_right_edge_partial() {
        assert_eq!(chunk_actual_size(1, 0, (1500, 1500), 1024), (476, 1024));
        assert_eq!(chunk_actual_size(1, 1, (1500, 1500), 1024), (476, 476));
    }

    #[test]
    fn chunk_actual_size_large_non_aligned() {
        assert_eq!(chunk_actual_size(16, 15, (16507, 16196), 1024), (123, 836));
        assert_eq!(chunk_actual_size(0, 0, (16507, 16196), 1024), (1024, 1024));
    }

    #[test]
    fn chunk_actual_size_image_multiple_of_chunk() {
        assert_eq!(chunk_actual_size(3, 2, (4096, 3072), 1024), (1024, 1024));
    }

    #[test]
    fn chunk_actual_size_indices_past_image_zero() {
        // Lock the contract: indices past the image clamp to zero (not negative).
        // chunks_intersecting clips before emitting, but a stray (cx, cy)
        // outside bounds must produce a degenerate, not panic.
        assert_eq!(chunk_actual_size(100, 100, (2048, 2048), 1024), (0, 0));
    }

    #[test]
    fn visibility_query_simple_single_instance() {
        let v = visibility_query(rect(100, 100, 100, 100), Point::from((0, 0)), (2048, 2048), 1024);
        assert_eq!(v, vec![(0, 0, 0, 0)]);
    }

    #[test]
    fn visibility_query_spans_two_instances_horizontally() {
        let v = visibility_query(rect(2000, 100, 100, 100), Point::from((0, 0)), (2048, 2048), 1024);
        assert!(v.contains(&(1, 0, 0, 0)));
        assert!(v.contains(&(0, 0, 1, 0)));
        assert_eq!(v.len(), 2);
    }

    #[test]
    fn visibility_query_empty_viewport_empty_result() {
        let v = visibility_query(rect(100, 100, 0, 0), Point::from((0, 0)), (2048, 2048), 1024);
        assert!(v.is_empty());
    }

    #[test]
    fn visibility_query_negative_viewport_with_wrap() {
        let v = visibility_query(rect(-100, 0, 200, 100), Point::from((0, 0)), (2048, 2048), 1024);
        assert!(v.contains(&(1, 0, -1, 0)));
        assert!(v.contains(&(0, 0, 0, 0)));
        assert_eq!(v.len(), 2);
    }

    #[test]
    fn pick_lod_at_native_zoom_returns_zero() {
        assert_eq!(pick_lod(1.0, 5), 0);
        assert_eq!(pick_lod(2.0, 5), 0);
    }

    #[test]
    fn pick_lod_descends_with_zoom() {
        assert_eq!(pick_lod(0.5, 5), 1);
        assert_eq!(pick_lod(0.25, 5), 2);
        assert_eq!(pick_lod(0.125, 5), 3);
    }

    #[test]
    fn pick_lod_just_below_power_of_two() {
        // Inter-LOD transitions use ceil semantics — at zoom 0.49 we're one
        // power-of-2 step below LOD 1's native zoom of 0.5, so LOD 2.
        assert_eq!(pick_lod(0.49, 5), 2);
        // zoom 0.51 just above 0.5 → ceil(0.971) = 1.
        assert_eq!(pick_lod(0.51, 5), 1);
    }

    #[test]
    fn pick_lod_last_boundary_is_power_of_two() {
        // n_lods=5, last=4 → fallback boundary at zoom = 2^-3 = 0.125,
        // a full octave finer than the previous half-octave geometric
        // mean. Trade: skip the painful coarsest-per-tile band where
        // per-viewport tile counts spike.
        assert_eq!(pick_lod(0.13, 5), 3);
        assert_eq!(pick_lod(0.12, 5), 4);
    }

    #[test]
    fn pick_lod_clamps_to_last() {
        // zoom = 0.01 → -log2(0.01) ≈ 6.64 → floor 6 → clamp to last = 4.
        assert_eq!(pick_lod(0.01, 5), 4);
    }

    #[test]
    fn pick_lod_non_finite_returns_zero() {
        // NaN/Infinity from buggy animation math collapse to 0 rather than
        // panic on the `as u32` cast.
        assert_eq!(pick_lod(f64::NAN, 5), 0);
        assert_eq!(pick_lod(f64::INFINITY, 5), 0);
        assert_eq!(pick_lod(f64::NEG_INFINITY, 5), 0);
    }

    fn meta(t: u64, bytes: u64) -> ChunkMeta {
        ChunkMeta {
            bytes,
            last_touched_frame: t,
        }
    }

    #[test]
    fn evict_lru_noop_under_budget() {
        let mut m: HashMap<(u32, i32, i32), ChunkMeta> = [
            ((0, 0, 0), meta(10, 100)),
            ((0, 1, 0), meta(20, 100)),
        ]
        .into_iter()
        .collect();
        let mut bytes: u64 = 200;
        let ev = evict_lru_to_budget(&mut m, &mut bytes, 1000);
        assert!(ev.is_empty());
        assert_eq!(bytes, 200);
        assert_eq!(m.len(), 2);
    }

    #[test]
    fn evict_lru_drops_oldest_first() {
        let mut m: HashMap<(u32, i32, i32), ChunkMeta> = [
            ((0, 0, 0), meta(5, 100)),
            ((0, 1, 0), meta(7, 100)),
            ((0, 2, 0), meta(9, 100)),
        ]
        .into_iter()
        .collect();
        let mut bytes: u64 = 300;
        let ev = evict_lru_to_budget(&mut m, &mut bytes, 100);
        assert_eq!(ev.len(), 2);
        assert_eq!(ev[0], (0, 0, 0));
        assert_eq!(ev[1], (0, 1, 0));
        assert_eq!(bytes, 100);
        assert!(m.contains_key(&(0, 2, 0)));
    }

    #[test]
    fn evict_lru_stops_at_budget_boundary() {
        let mut m: HashMap<(u32, i32, i32), ChunkMeta> = [
            ((0, 0, 0), meta(5, 100)),
            ((0, 1, 0), meta(7, 100)),
        ]
        .into_iter()
        .collect();
        let mut bytes: u64 = 200;
        let ev = evict_lru_to_budget(&mut m, &mut bytes, 100);
        assert_eq!(ev, vec![(0, 0, 0)]);
        assert_eq!(bytes, 100);
        assert!(m.contains_key(&(0, 1, 0)));
    }

    #[test]
    fn evict_lru_handles_byte_saturation() {
        // Stored bytes wildly exceed actual sum — saturating_sub keeps it at 0.
        let mut m: HashMap<(u32, i32, i32), ChunkMeta> =
            [((0, 0, 0), meta(5, u64::MAX))].into_iter().collect();
        let mut bytes: u64 = 50;
        let ev = evict_lru_to_budget(&mut m, &mut bytes, 0);
        assert_eq!(ev, vec![(0, 0, 0)]);
        assert_eq!(bytes, 0);
    }

    #[test]
    fn evict_lru_empty_meta_is_noop() {
        let mut m: HashMap<(u32, i32, i32), ChunkMeta> = HashMap::new();
        let mut bytes: u64 = 0;
        let ev = evict_lru_to_budget(&mut m, &mut bytes, 0);
        assert!(ev.is_empty());
        assert_eq!(bytes, 0);
    }

    #[test]
    fn evict_lru_ties_preserve_at_least_one() {
        // `sort_unstable_by_key` is non-deterministic on ties, but the per-
        // iteration budget check guarantees we don't over-evict.
        let mut m: HashMap<(u32, i32, i32), ChunkMeta> = [
            ((0, 0, 0), meta(5, 100)),
            ((0, 1, 0), meta(5, 100)),
        ]
        .into_iter()
        .collect();
        let mut bytes: u64 = 200;
        let ev = evict_lru_to_budget(&mut m, &mut bytes, 100);
        assert_eq!(ev.len(), 1);
        assert_eq!(m.len(), 1);
        assert_eq!(bytes, 100);
    }

    #[test]
    fn pick_lod_zero_zoom_returns_last() {
        assert_eq!(pick_lod(0.0, 5), 4);
    }

    #[test]
    fn pick_lod_zero_lods_returns_zero() {
        // Defensive: caller invariant is n_lods >= 1, but the function
        // shouldn't panic if the invariant breaks.
        assert_eq!(pick_lod(0.5, 0), 0);
    }

    #[test]
    fn aggregations_lod_zero_uses_lod0_target() {
        // LOD 0 aggregates against the smaller lod0_target (256 → 1024/256 = 4),
        // not the coarser-LOD target.
        let agg = compute_aggregations(&[256, 512, 1024, 2048], 2048, 1024);
        assert_eq!(agg[0], 4);
    }

    #[test]
    fn aggregations_typical_43k_image() {
        let raw = vec![256, 512, 1024, 2048, 4096, 8192, 16384, 32768];
        let agg = compute_aggregations(&raw, 2048, 1024);
        assert_eq!(agg, vec![4, 4, 2, 1, 1, 1, 1, 1]);
    }

    #[test]
    fn aggregations_final_sizes_monotonic_integer_ratios() {
        // Adjacent final-size ratios must be integer powers of 2 — the
        // coarser-LOD fallback's `canvas_x.div_euclid(size)` lookup needs it.
        let raw = vec![256, 512, 1024, 2048, 4096, 8192];
        let agg = compute_aggregations(&raw, 2048, 1024);
        let sizes: Vec<i32> = raw.iter().zip(&agg).map(|(r, a)| r * *a as i32).collect();
        assert_eq!(sizes, vec![1024, 2048, 2048, 2048, 4096, 8192]);
        for i in 1..sizes.len() {
            let ratio = sizes[i] as f64 / sizes[i - 1] as f64;
            assert!(
                ratio == 1.0 || ratio == 2.0 || ratio == 4.0 || ratio == 8.0,
                "LOD {i} ratio {ratio} is not a power of 2"
            );
            assert!(
                sizes[i] % sizes[i - 1] == 0,
                "LOD {i} ({}) not divisible by LOD {} ({})",
                sizes[i],
                i - 1,
                sizes[i - 1]
            );
        }
    }

    #[test]
    fn aggregations_target_below_raw_keeps_one() {
        let agg = compute_aggregations(&[1024, 2048, 4096], 1024, 1024);
        assert_eq!(agg, vec![1, 1, 1]);
    }

    #[test]
    fn aggregations_empty_input() {
        assert_eq!(compute_aggregations(&[], 2048, 1024), Vec::<u32>::new());
    }

    #[test]
    fn aggregations_zero_raw_clamps_to_one() {
        // Guard against div-by-zero on a pathological raw=0 entry. Other LODs
        // still aggregate: LOD 0 by lod0_target (1024/256=4), LOD 2 by the
        // coarser-LOD target (2048/1024=2).
        let agg = compute_aggregations(&[256, 0, 1024], 2048, 1024);
        assert_eq!(agg[1], 1);
        assert_eq!(agg[0], 4);
        assert_eq!(agg[2], 2);
    }

    #[test]
    fn aggregations_single_lod() {
        // Only LOD 0 present (tiny single-page TIFF): no panic, aggregates by
        // lod0_target.
        assert_eq!(compute_aggregations(&[256], 2048, 1024), vec![4]);
    }

    #[test]
    fn visibility_query_centered_image() {
        // image_position = -dims/2 → image centered on canvas (0,0). Viewport
        // around the origin should hit the inner chunks of instance k=0.
        let v = visibility_query(
            rect(-50, -50, 100, 100),
            Point::from((-1024, -1024)),
            (2048, 2048),
            1024,
        );
        // Instance k=0 origin is (-1024, -1024). Viewport relative to it:
        // [974, 1074) × [974, 1074) → straddles chunk (0,0) and (1,1).
        assert!(v.contains(&(0, 0, 0, 0)));
        assert!(v.contains(&(1, 0, 0, 0)));
        assert!(v.contains(&(0, 1, 0, 0)));
        assert!(v.contains(&(1, 1, 0, 0)));
        assert_eq!(v.len(), 4);
    }
}
