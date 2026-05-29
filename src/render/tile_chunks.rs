//! Chunked tile-background rendering. `BgChunkCache` owns a [`TiffSource`] and
//! decodes+uploads tiles on demand at a zoom-selected LOD. When the requested
//! fine LOD isn't cached, [`chunk_render_elements`] falls back to the coarsest
//! cached LOD that covers the region (blurry-then-sharp loading).

use std::collections::{HashMap, HashSet};
use std::ops::Range;

use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::{ImportMem, Texture};
use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::gles::{GlesRenderer, GlesTexProgram, GlesTexture};
use smithay::utils::{Buffer, Logical, Physical, Point, Rectangle, Size};

use super::tile_chunks_tiff::TiffSource;
use super::{PixelSnapRescaleElement, TileShaderElement};

/// Hardcoded VRAM ceiling for cached chunked-bg tiles. Matches the `image`
/// crate's default decompression-bomb cap — same order of magnitude as the
/// memory a "reasonable" wallpaper consumes — and lets ~2000 256×256 RGBA8
/// tiles coexist, easily covering visible+nearby at any zoom.
const VRAM_BUDGET_BYTES: u64 = 512 * 1024 * 1024;

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
    pub chunks: HashMap<(u32, i32, i32), GlesTexture>,
    /// A coarse-LOD fallback tile can show up here keyed at its own `lod` even
    /// when the requesting fine LOD has no element of its own, so the key
    /// space spans all LODs, not just the currently-selected one.
    pub chunk_elements: HashMap<(u32, i32, i32, i32, i32), TileShaderElement>,
    /// Per-LOD chunk canvas span (canvas units per tile at that LOD). Computed
    /// once at init so lookup and emission read the same rounded values, and
    /// validated to a strict 2× ratio between adjacent LODs — coarser-LOD
    /// fallback's `div_euclid(2)` map then has no straddle gaps.
    chunk_canvas_sizes: Vec<i32>,
    /// Parallel to `chunks`: tracks per-tile VRAM and last-touched frame for
    /// LRU eviction. Kept separate so external readers of `chunks` see the
    /// textures directly.
    chunk_meta: HashMap<(u32, i32, i32), ChunkMeta>,
    vram_bytes: u64,
    /// Advanced once per frame; used only as a relative LRU timestamp.
    /// u64::MAX wraparound is millennia at 60 fps — ignored.
    frame_counter: u64,
    source: TiffSource,
}

impl BgChunkCache {
    /// Caller must run [`ensure_visible_loaded`](Self::ensure_visible_loaded)
    /// before [`chunk_render_elements`] each frame — otherwise freshly visible
    /// tiles silently no-op and the screen stays blank until the next call.
    pub fn new_from_tiff(
        source: TiffSource,
        chunk_bg_shader: GlesTexProgram,
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
        let chunk_canvas_sizes = derive_chunk_canvas_sizes(&source)?;
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
            chunk_meta: HashMap::new(),
            vram_bytes: 0,
            frame_counter: 0,
            source,
        })
    }

    pub fn n_lods(&self) -> u32 {
        self.chunk_canvas_sizes.len() as u32
    }

    pub fn chunk_canvas_size_at(&self, lod: u32) -> i32 {
        self.chunk_canvas_sizes[lod as usize]
    }

    /// Per-frame entry point: bump LRU clock, stamp visible tiles and their
    /// coarser-LOD fallback ancestors, then upload up to `budget` new fine
    /// tiles and evict over-budget LRU. Over-budget tiles get reconsidered
    /// next frame; failed decodes are logged so the caller retries.
    pub fn ensure_visible_loaded(
        &mut self,
        viewport: Rectangle<i32, Logical>,
        renderer: &mut GlesRenderer,
        zoom: f64,
        budget: usize,
    ) -> usize {
        self.frame_counter = self.frame_counter.wrapping_add(1);
        let target_lod = pick_lod(zoom, self.n_lods());
        let target_chunk_size = self.chunk_canvas_size_at(target_lod);
        let visible = visibility_query(
            viewport,
            self.image_position,
            self.image_dims,
            target_chunk_size,
        );
        // Dedup (cx, cy) across wrap offsets: one source tile maps to many
        // canvas instances but uploads once.
        let mut wanted: HashSet<(i32, i32)> = HashSet::with_capacity(visible.len());
        for (cx, cy, _kx, _ky) in &visible {
            wanted.insert((*cx, *cy));
        }

        // Stamp coarser-LOD fallback ancestors so eviction can't drop the
        // coarse cover while the fine LOD is still loading — would flash blank.
        let n_lods = self.n_lods();
        let frame = self.frame_counter;
        for (cx_t, cy_t) in &wanted {
            let canvas_x = cx_t * target_chunk_size;
            let canvas_y = cy_t * target_chunk_size;
            for try_lod in target_lod..n_lods {
                let try_size = self.chunk_canvas_size_at(try_lod);
                let try_cx = canvas_x.div_euclid(try_size);
                let try_cy = canvas_y.div_euclid(try_size);
                if let Some(m) = self.chunk_meta.get_mut(&(try_lod, try_cx, try_cy)) {
                    m.last_touched_frame = frame;
                }
            }
        }

        let mut loaded = 0;
        for (cx, cy) in wanted {
            if loaded >= budget {
                break;
            }
            if self.chunks.contains_key(&(target_lod, cx, cy)) {
                continue;
            }
            // visibility_query clips to image bounds so cx/cy >= 0.
            let Ok(cx_u) = u32::try_from(cx) else { continue };
            let Ok(cy_u) = u32::try_from(cy) else { continue };
            match self.load_tile(target_lod, cx_u, cy_u, renderer) {
                Ok(tex) => {
                    // 4 bytes/texel estimate for RGBA8 — drivers may pad small
                    // textures up; accept 5-10% slop in the budget.
                    let bytes = (tex.width() as u64) * (tex.height() as u64) * 4;
                    let key = (target_lod, cx, cy);
                    self.chunks.insert(key, tex);
                    self.vram_bytes = self.vram_bytes.saturating_add(bytes);
                    self.chunk_meta.insert(
                        key,
                        ChunkMeta {
                            bytes,
                            last_touched_frame: frame,
                        },
                    );
                    loaded += 1;
                }
                Err(e) => {
                    tracing::warn!(
                        "chunked tile (LOD {target_lod}, {cx},{cy}) load failed: {e}"
                    );
                }
            }
        }
        self.evict_over_budget();
        loaded
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

    fn load_tile(
        &mut self,
        lod: u32,
        cx: u32,
        cy: u32,
        renderer: &mut GlesRenderer,
    ) -> Result<GlesTexture, String> {
        let tile = self.source.read_tile(lod, cx, cy)?;
        renderer
            .import_memory(
                &tile.rgba,
                Fourcc::Abgr8888,
                Size::<i32, Buffer>::from((tile.width as i32, tile.height as i32)),
                false,
            )
            .map_err(|e| {
                format!("import_memory ({}x{}): {e}", tile.width, tile.height)
            })
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

/// Pick the largest LOD whose sample density still matches or exceeds what the
/// screen needs at this zoom — `k = floor(-log2(zoom))`. Clamped to
/// `[0, n_lods - 1]`. At `zoom >= 1.0` always returns LOD 0. NaN/non-finite
/// zoom returns LOD 0 (defensive — animation math shouldn't produce NaN,
/// but a clamp prevents a panic from `as u32` on a NaN cast).
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
    let n = (-zoom.log2()).floor();
    if !n.is_finite() || n < 0.0 {
        return 0;
    }
    (n as u32).min(last)
}

/// Compute per-LOD canvas chunk sizes (canvas units per tile at that LOD) and
/// validate that adjacent LODs sit at a strict 2× ratio. Most pyramidal-TIFF
/// converters (vips, gdal) output power-of-2 pyramids; non-2× pyramids would
/// require multi-coarse-tile lookup to avoid straddle gaps in the fallback
/// path, which isn't worth the complexity. Reject at init instead.
fn derive_chunk_canvas_sizes(source: &TiffSource) -> Result<Vec<i32>, String> {
    let lods = source.lods();
    let lod_0_w = lods[0].image_dims.0 as f64;
    let mut sizes = Vec::with_capacity(lods.len());
    for (i, meta) in lods.iter().enumerate() {
        let scale = lod_0_w / meta.image_dims.0 as f64;
        let s = (meta.tile_dims.0 as f64 * scale).round() as i32;
        if s <= 0 {
            return Err(format!("LOD {i}: derived canvas chunk size {s} <= 0"));
        }
        sizes.push(s);
    }
    for i in 1..sizes.len() {
        if sizes[i] != 2 * sizes[i - 1] {
            return Err(format!(
                "non-power-of-2 LOD pyramid: LOD {i} canvas chunk size {} \
                 must be 2× LOD {} size {} (use a vips/gdal-style 2× pyramid)",
                sizes[i],
                i - 1,
                sizes[i - 1],
            ));
        }
    }
    Ok(sizes)
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

/// Per-frame visibility update + element create/reuse.
///
/// Selects the target LOD from `zoom`, then for each visible fine chunk walks
/// progressively coarser cached LODs until one is found (or skips if none).
/// The coarser tile renders over a LARGER canvas area than the fine chunk
/// asked for; adjacent fine chunks that resolve to the same coarse tile
/// dedup to a single element via the `(lod, cx, cy, kx, ky)` element key.
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
    let target_lod = pick_lod(zoom, cache.n_lods());
    let target_chunk_size = cache.chunk_canvas_size_at(target_lod);
    let visible_target = visibility_query(
        viewport,
        cache.image_position,
        cache.image_dims,
        target_chunk_size,
    );

    let n_lods = cache.n_lods();
    let mut to_render: HashSet<(u32, i32, i32, i32, i32)> =
        HashSet::with_capacity(visible_target.len());
    for (cx_t, cy_t, kx, ky) in &visible_target {
        // (kx, ky) factors into element placement, not LOD lookup — coarser
        // LODs are picked on the k=0-instance canvas origin.
        let target_cx_canvas = cx_t * target_chunk_size;
        let target_cy_canvas = cy_t * target_chunk_size;
        for try_lod in target_lod..n_lods {
            let try_size = cache.chunk_canvas_size_at(try_lod);
            let try_cx = target_cx_canvas.div_euclid(try_size);
            let try_cy = target_cy_canvas.div_euclid(try_size);
            if cache.chunks.contains_key(&(try_lod, try_cx, try_cy)) {
                to_render.insert((try_lod, try_cx, try_cy, *kx, *ky));
                break;
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
    // deterministic, and overlapping coarse tiles (LOD-boundary fallback)
    // would otherwise z-fight frame-to-frame.
    let mut ordered: Vec<_> = to_render.into_iter().collect();
    ordered.sort_unstable();

    let mut out = Vec::with_capacity(ordered.len());
    for key in ordered {
        let (lod, cx, cy, kx, ky) = key;
        let chunk_size = cache.chunk_canvas_size_at(lod);
        // Edge chunks (right/bottom) may be smaller when image_dims isn't a
        // multiple of chunk_size at this LOD. Use the actual size for the
        // canvas area so edge chunks don't stretch past the next wrap-offset
        // instance. Texture pixel dims come from the GPU texture itself
        // (`tex.width()/height()`) — at coarser LODs they're FAR smaller than
        // the canvas span, and passing canvas units as tex_w/tex_h would make
        // the shader sample a tiny corner of the texture into a huge area.
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
        // zoom = 0.49 → -log2(0.49) ≈ 1.03 → floor 1 → LOD 1 (still sharp
        // enough; LOD 1 pixel covers 0.98 screen px at this zoom).
        assert_eq!(pick_lod(0.49, 5), 1);
        // zoom = 0.51 just above 0.5 → -log2 ≈ 0.97 → floor 0 → LOD 0.
        assert_eq!(pick_lod(0.51, 5), 0);
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
