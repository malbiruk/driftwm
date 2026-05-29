//! Chunked tile-background rendering. `BgChunkCache` owns a [`TiffSource`] and
//! decodes+uploads tiles on demand; element create/reuse emits only what's
//! already cached.

use std::collections::{HashMap, HashSet};
use std::ops::Range;

use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::ImportMem;
use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::gles::{GlesRenderer, GlesTexProgram, GlesTexture};
use smithay::utils::{Buffer, Logical, Physical, Point, Rectangle, Size};

use super::tile_chunks_tiff::TiffSource;
use super::{PixelSnapRescaleElement, TileShaderElement};

pub struct BgChunkCache {
    pub image_dims: (u32, u32),
    /// Canvas-coord position of the image's top-left corner. Set so the image
    /// is *centered* on canvas (0, 0) — i.e. `(-image_w/2, -image_h/2)`. Wrap
    /// seams then sit at ±image_w/2, ±image_h/2 rather than at the origin.
    pub image_position: Point<i32, Logical>,
    pub chunk_canvas_size: i32,
    pub chunk_bg_shader: GlesTexProgram,
    pub chunks: HashMap<(i32, i32), GlesTexture>,
    pub chunk_elements: HashMap<(i32, i32, i32, i32), TileShaderElement>,
    source: TiffSource,
}

// Silenced until init_background's TIFF routing lands.
#[allow(dead_code)]
impl BgChunkCache {
    /// Caller must run [`ensure_visible_loaded`](Self::ensure_visible_loaded)
    /// before [`chunk_render_elements`] each frame — otherwise freshly visible
    /// tiles silently no-op and the screen stays blank until the next call.
    pub fn new_from_tiff(
        source: TiffSource,
        chunk_bg_shader: GlesTexProgram,
    ) -> Result<Self, String> {
        let meta = *source
            .lods()
            .first()
            .ok_or("TIFF has no LOD levels")?;
        if meta.tile_dims.0 != meta.tile_dims.1 {
            return Err(format!(
                "non-square tile dims {:?} not supported",
                meta.tile_dims
            ));
        }
        let image_dims = meta.image_dims;
        let image_position = Point::from((
            -(image_dims.0 as i32) / 2,
            -(image_dims.1 as i32) / 2,
        ));
        Ok(Self {
            image_dims,
            image_position,
            chunk_canvas_size: meta.tile_dims.0 as i32,
            chunk_bg_shader,
            chunks: HashMap::new(),
            chunk_elements: HashMap::new(),
            source,
        })
    }

    /// Decode + upload up to `budget` not-yet-cached visible tiles; over-budget
    /// tiles are reconsidered next frame. Failed decodes are logged and
    /// dropped so the caller retries.
    pub fn ensure_visible_loaded(
        &mut self,
        viewport: Rectangle<i32, Logical>,
        renderer: &mut GlesRenderer,
        budget: usize,
    ) -> usize {
        let visible = visibility_query(
            viewport,
            self.image_position,
            self.image_dims,
            self.chunk_canvas_size,
        );
        // Dedup (cx, cy) across wrap offsets: one image tile maps to many
        // canvas instances but uploads once.
        let mut wanted: HashSet<(i32, i32)> = HashSet::with_capacity(visible.len());
        for (cx, cy, _kx, _ky) in &visible {
            wanted.insert((*cx, *cy));
        }
        let mut loaded = 0;
        for (cx, cy) in wanted {
            if loaded >= budget {
                break;
            }
            if self.chunks.contains_key(&(cx, cy)) {
                continue;
            }
            // visibility_query clips to image bounds so cx/cy >= 0.
            let Ok(cx_u) = u32::try_from(cx) else { continue };
            let Ok(cy_u) = u32::try_from(cy) else { continue };
            match self.load_tile(cx_u, cy_u, renderer) {
                Ok(tex) => {
                    self.chunks.insert((cx, cy), tex);
                    loaded += 1;
                }
                Err(e) => {
                    tracing::warn!("chunked tile ({cx},{cy}) load failed: {e}");
                }
            }
        }
        loaded
    }

    fn load_tile(
        &mut self,
        cx: u32,
        cy: u32,
        renderer: &mut GlesRenderer,
    ) -> Result<GlesTexture, String> {
        let tile = self.source.read_tile(0, cx, cy)?;
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
/// * Inner `TileShaderElement` is persistent per `(cx, cy, kx, ky)` with a stable
///   `Id`; `resize()` is idempotent so a static camera leaves the commit counter
///   alone and the damage tracker preserves the frame.
/// * All chunks share a `(0, 0)` rounding anchor in `PixelSnapRescaleElement` so
///   adjacent chunks meet at pixel-consistent edges at fractional zoom — actual
///   position lives in the inner element's `area`.
pub fn chunk_render_elements(
    cache: &mut BgChunkCache,
    viewport: Rectangle<i32, Logical>,
    camera: Point<f64, Logical>,
    zoom: f64,
) -> Vec<PixelSnapRescaleElement<TileShaderElement>> {
    let visible = visibility_query(
        viewport,
        cache.image_position,
        cache.image_dims,
        cache.chunk_canvas_size,
    );
    let visible_set: HashSet<(i32, i32, i32, i32)> = visible.iter().copied().collect();
    cache
        .chunk_elements
        .retain(|key, _| visible_set.contains(key));

    let camera_i = Point::<i32, Logical>::from((camera.x.round() as i32, camera.y.round() as i32));
    let chunk_size = cache.chunk_canvas_size;
    let image_dims = cache.image_dims;
    let image_position = cache.image_position;

    let mut out = Vec::with_capacity(visible.len());
    for key in visible {
        let (cx, cy, kx, ky) = key;
        // Edge chunks (right/bottom) may be smaller than `chunk_size` when
        // `image_dims` isn't a multiple of `chunk_size`. Use the actual size
        // for both the canvas area and the texture sample rect — otherwise
        // edge chunks get stretched to `chunk_size` and overlap the next
        // wrap-offset instance.
        let (chunk_w, chunk_h) = chunk_actual_size(cx, cy, image_dims, chunk_size);
        let canvas_origin =
            chunk_canvas_origin(cx, cy, kx, ky, image_position, image_dims, chunk_size);
        let area = Rectangle::new(
            Point::from((canvas_origin.x - camera_i.x, canvas_origin.y - camera_i.y)),
            Size::from((chunk_w, chunk_h)),
        );
        let opaque = vec![area];

        // Source-chunk grid is fully populated by the loader; a missing
        // (cx, cy) would be a bug. Skip rather than panic.
        if !cache.chunks.contains_key(&(cx, cy)) {
            continue;
        }
        let elem = cache.chunk_elements.entry(key).or_insert_with(|| {
            // Cloned inside the closure so cache-hit iterations skip the
            // refcount bump on `tex` and the shader.
            let tex = cache.chunks.get(&(cx, cy)).unwrap().clone();
            TileShaderElement::new(
                cache.chunk_bg_shader.clone(),
                tex,
                chunk_w,
                chunk_h,
                area,
                Some(opaque.clone()),
                1.0,
                vec![],
                Kind::Unspecified,
            )
        });
        // Idempotent when `area` is unchanged — no commit bump, no damage.
        // `camera_i` is integer-rounded, so sub-pixel camera drift (<= 0.5
        // logical px) produces the same `area` and the same no-damage path.
        elem.resize(area, Some(opaque));
        out.push(PixelSnapRescaleElement::from_element(
            elem.clone(),
            // Shared rounding anchor (not image_position) — see fn doc.
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
