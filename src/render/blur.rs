use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::element::texture::TextureRenderElement;
use smithay::backend::renderer::element::utils::{Relocate, RelocateRenderElement};
use smithay::backend::renderer::element::{Element, Id, Kind};
use smithay::backend::renderer::gles::{
    GlesError, GlesRenderer, GlesTexProgram, GlesTexture, Uniform, UniformName, UniformType,
};
use smithay::backend::renderer::utils::{CommitCounter, DamageBag};
use smithay::output::Output;
use smithay::utils::{Buffer, Logical, Physical, Point, Rectangle, Size, Transform};
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use super::OutputRenderElements;

static BLUR_DOWN_SRC: &str = include_str!("../shaders/blur_down.glsl");
static BLUR_UP_SRC: &str = include_str!("../shaders/blur_up.glsl");

/// Both facts one walk over what lies beneath a window decides.
struct BackdropFingerprint {
    hash: u64,
    /// Something between this window and the scene background reaches into the
    /// rect its frost is captured from. The shared backdrop holds the scene
    /// background alone, so this window cannot slice its frost out of it and
    /// renders its own.
    occluded_by_lower: bool,
}

/// Fingerprint of everything one window's own frost is actually taken from,
/// split at the scene background: `lower` is what lies between the window and
/// that background — other windows, widgets, canvas layers, each with its
/// geometry — and `background` the scene background itself.
///
/// Two asymmetries, both load-bearing.
///
/// Commit counters count above the split and are dropped below it. A client
/// redrawing beneath a frosted window keeps one long-lived element `Id`, so
/// identity alone leaves the frost frozen over playing video. The scene
/// background redraws under long-lived `Id`s too, but its content cadence is
/// already the shared backdrop's beat, and that beat is what `animate_blur_fps`
/// caps — taking its commits here would recompute every window on this path at
/// the background's own frame rate instead, uncapped, for exactly the windows
/// the cap exists to protect.
///
/// The lower elements are also filtered to the padded capture rect, and the
/// same filter answers `occluded_by_lower`, so what can change this window's
/// frost and what can force it off the shared backdrop cannot disagree. An
/// element beside the window rather than under it is in neither: a video
/// playing next to a frosted window would otherwise re-blur it at 60 fps for a
/// pixel-identical result.
fn backdrop_fingerprint<'a>(
    lower: impl Iterator<Item = (&'a Id, CommitCounter, Rectangle<i32, Physical>)>,
    background: impl ExactSizeIterator<Item = (&'a Id, CommitCounter)>,
    window_rect: Rectangle<i32, Physical>,
    pad: i32,
    region_rects: Option<&[Rectangle<i32, Physical>]>,
) -> BackdropFingerprint {
    let capture = padded_rect(window_rect, pad);
    let mut hasher = DefaultHasher::new();
    let mut lower_count = 0usize;
    for (id, commit, geometry) in lower {
        if !geometry.overlaps(capture) {
            continue;
        }
        lower_count += 1;
        id.hash(&mut hasher);
        // `CommitCounter` is opaque and not `Hash`; its distance from a zero
        // counter is the count itself.
        commit
            .distance(Some(CommitCounter::default()))
            .hash(&mut hasher);
    }
    // Of what survived the filter, so a window mapping or unmapping out of
    // reach stays as invisible as its redraws.
    lower_count.hash(&mut hasher);
    background.len().hash(&mut hasher);
    for (id, _commit) in background {
        id.hash(&mut hasher);
    }
    window_rect.loc.x.hash(&mut hasher);
    window_rect.loc.y.hash(&mut hasher);
    window_rect.size.w.hash(&mut hasher);
    window_rect.size.h.hash(&mut hasher);
    // Hash by content, not Arc identity — a fresh Arc with identical rects
    // shouldn't invalidate the cache.
    if let Some(rects) = region_rects {
        rects.len().hash(&mut hasher);
        for r in rects {
            r.loc.x.hash(&mut hasher);
            r.loc.y.hash(&mut hasher);
            r.size.w.hash(&mut hasher);
            r.size.h.hash(&mut hasher);
        }
    }
    BackdropFingerprint {
        hash: hasher.finish(),
        occluded_by_lower: lower_count > 0,
    }
}

/// Fingerprint of the scene background, which is what the shared backdrop
/// captures. Commit counters, not element `Id`s alone: a wallpaper daemon draws
/// every new frame into one long-lived surface, so its `Id` never changes and
/// an Id-only comparison leaves the frost frozen over it.
fn background_signature(elements: &[OutputRenderElements]) -> Vec<(Id, CommitCounter)> {
    elements
        .iter()
        .map(|e| (e.id().clone(), e.current_commit()))
        .collect()
}

/// Whether the background changed *structurally* — an element added, removed,
/// or reordered — rather than an existing one drawing new content.
///
/// The two are paced differently. New content in the same elements is the
/// background animating, which is what `animate_blur_fps` exists to cap; a
/// changed element set is a wallpaper daemon restarting, a Background-layer
/// surface mapping or unmapping, a wallpaper swap. Capping that would leave the
/// backdrop showing a background that is no longer on screen — at
/// `animate_blur_fps = 0`, until the camera happened to move.
fn background_structure_changed(
    before: &[(Id, CommitCounter)],
    after: &[(Id, CommitCounter)],
) -> bool {
    before.len() != after.len() || before.iter().zip(after).any(|(a, b)| a.0 != b.0)
}

/// The view a captured backdrop is only valid at.
///
/// Held as the f64 camera the scene is actually drawn from, not as a counter
/// bumped off the rounded camera the output geometry is mapped from: an easing
/// tail drifts the camera most of a canvas unit — `zoom * output_scale`
/// physical pixels — between two rounded values, and on those frames the
/// backdrop would stand still while the windows slicing it moved.
///
/// The seed is NaN so a never-captured backdrop compares unequal to every view.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ViewStamp {
    pub camera: Point<f64, Logical>,
    pub zoom: f64,
}

impl ViewStamp {
    fn never_captured() -> Self {
        Self {
            camera: Point::from((f64::NAN, f64::NAN)),
            zoom: f64::NAN,
        }
    }
}

/// Extent of the padded ping-pong pair: the live window extent grown by the
/// blur reach on every side.
///
/// Deliberately not quantized, unlike the textures the result is kept in. The
/// Kawase taps read past the live edge and the wrap mode mirrors about the
/// *allocation* edge, so an allocation larger than the live extent leaves a
/// strip that no pass ever writes — `create_buffer` hands it over undefined —
/// exactly where the taps land.
fn pad_extent(win_size: Size<i32, Physical>, pad: i32) -> Size<i32, Physical> {
    (win_size.w + 2 * pad, win_size.h + 2 * pad).into()
}

/// The screen rect a window's frost is captured from: its own rect grown by the
/// blur reach. One constructor, because every decision taken about this rect —
/// what lands in the frost, what invalidates it, what the shared backdrop would
/// have to render instead — is only consistent while they all mean the same
/// rect.
fn padded_rect(win_rect: Rectangle<i32, Physical>, pad: i32) -> Rectangle<i32, Physical> {
    Rectangle::new(
        (win_rect.loc.x - pad, win_rect.loc.y - pad).into(),
        pad_extent(win_rect.size, pad),
    )
}

/// What a captured mask is only valid for. The mask is the window's alpha
/// shape rasterized at the live on-screen extent, so it goes stale on anything
/// that moves that shape — geometry, the extent itself (i.e. zoom), and the
/// client's blur region, which lives in the mask and nowhere else.
pub struct MaskStamp {
    geometry_generation: u64,
    live: Size<i32, Physical>,
    regions: Option<Vec<Rectangle<i32, Physical>>>,
}

impl MaskStamp {
    fn matches(
        &self,
        geometry_generation: u64,
        live: Size<i32, Physical>,
        regions: Option<&[Rectangle<i32, Physical>]>,
    ) -> bool {
        self.geometry_generation == geometry_generation
            && self.live == live
            && self.regions.as_deref() == regions
    }
}

/// Per-window cached textures for Kawase blur ping-pong passes.
pub struct BlurCache {
    pub texture: GlesTexture,
    pub mask: GlesTexture,
    /// Padded ping-pong pair for the exact per-window blur path. Blurring
    /// exactly the window rect makes edge samples clamp to the border
    /// pixels, smearing the backdrop inward as a bevel-like band, so the
    /// blur runs on a padded crop and only the centre is kept.
    /// Lazy: the shared path never needs them, and allocating them eagerly
    /// wastes ~10 MB per frosted window.
    pub pads: Option<(GlesTexture, GlesTexture)>,
    /// Extent of the pad pair. Exactly [`pad_extent`], never quantized — see
    /// there for why.
    pub pad_alloc: Size<i32, Physical>,
    /// Consecutive frames this window has not run the per-window path. The pads
    /// are the largest allocation here, and a window that settles onto the
    /// shared slice would otherwise hold them for as long as it lives.
    pub pads_idle_frames: u32,
    /// Allocated extent of `texture` and `mask` — the live on-screen extent
    /// quantized up (see [`quantized_alloc`]), so a zoom stops reallocating
    /// them per frame. Every render into them covers only the live sub-rect at
    /// the top-left corner; every read of either — the mask multiply, the
    /// on-screen element — names the live extent as its `src`, and
    /// [`zero_texture`] covers the sliver a fractional scale can still reach.
    /// The pads cannot be quantized this way; see [`pad_extent`].
    pub alloc: Size<i32, Physical>,
    /// What the mask currently in `mask` was captured for, `None` until it has
    /// been captured at all. Animated refreshes reuse it instead of
    /// re-rendering the surface per window per tick.
    pub mask_stamp: Option<MaskStamp>,
    /// Whether `texture` is known to hold nothing — freshly allocated, or
    /// zeroed because there was no backdrop left to frost. The element is left
    /// out of the frame entirely while this holds: it would draw nothing, and a
    /// fully transparent element still counts against the one the primary plane
    /// needs to see to scan a fullscreen window out directly.
    pub zeroed: bool,
    pub dirty: bool,
    pub last_geometry_generation: u64,
    /// View the blur was last computed at.
    pub last_view: ViewStamp,
    pub last_backdrop_hash: u64,
    /// Stable element identity across frames. The damage tracker treats elements
    /// with unknown Ids as fully damaged — a fresh Id per frame defeats caching.
    pub id: Id,
    /// Records damage only when the blur texture is actually recomputed.
    /// Cache-hit frames leave this untouched, so the tracker sees zero damage.
    pub damage_bag: DamageBag<i32, Buffer>,
    /// Force-dirty countdown for the first few frames after creation.
    /// Clients backing surfaces with DMA-BUF (GTK4, fuzzel, swaync) finish
    /// their async texture import a frame or two after the surface is mapped.
    /// If we compute the mask alpha capture before the import lands, the mask
    /// is empty alpha → the multiply zeros the blur → we cache an invisible
    /// blur that persists until something else (camera move, geometry change)
    /// invalidates the cache. Forcing a recompute for the next frame after
    /// creation gives the import time to settle.
    pub force_dirty_frames: u8,
}

impl BlurCache {
    pub fn new(renderer: &mut GlesRenderer, alloc: Size<i32, Physical>, wrap: i32) -> Option<Self> {
        use smithay::backend::renderer::Offscreen;
        let buf_size = alloc.to_logical(1).to_buffer(1, Transform::Normal);
        let t1 =
            Offscreen::<GlesTexture>::create_buffer(renderer, Fourcc::Abgr8888, buf_size).ok()?;
        let t3 =
            Offscreen::<GlesTexture>::create_buffer(renderer, Fourcc::Abgr8888, buf_size).ok()?;
        set_wrap_mode(renderer, &t1, wrap);
        set_wrap_mode(renderer, &t3, wrap);
        zero_texture(renderer, &t1, alloc);
        zero_texture(renderer, &t3, alloc);
        Some(Self {
            texture: t1,
            mask: t3,
            pads: None,
            pad_alloc: Size::default(),
            pads_idle_frames: 0,
            alloc,
            mask_stamp: None,
            zeroed: true,
            dirty: true,
            last_geometry_generation: 0,
            last_view: ViewStamp::never_captured(),
            last_backdrop_hash: 0,
            id: Id::new(),
            damage_bag: DamageBag::new(4),
            force_dirty_frames: 2,
        })
    }

    /// Create the padded ping-pong pair at `pad_live` — on first use of the
    /// exact path, and again whenever the live extent moves, since these two
    /// may not be sized to anything but the extent being blurred.
    pub fn ensure_pads(
        &mut self,
        renderer: &mut GlesRenderer,
        pad_live: Size<i32, Physical>,
        wrap: i32,
    ) -> bool {
        use smithay::backend::renderer::Offscreen;
        if self.pads.is_some() && self.pad_alloc == pad_live {
            return true;
        }
        let pad_buf_size = pad_live.to_logical(1).to_buffer(1, Transform::Normal);
        let a = Offscreen::<GlesTexture>::create_buffer(renderer, Fourcc::Abgr8888, pad_buf_size);
        let b = Offscreen::<GlesTexture>::create_buffer(renderer, Fourcc::Abgr8888, pad_buf_size);
        if let (Ok(a), Ok(b)) = (a, b) {
            // The blur passes sample past these textures' edges; the results land
            // in the padding ring and are cropped away, but the wrap mode still
            // has to be a legal one — an NPOT texture left at the GL default
            // REPEAT is incomplete on GLES 2 and samples black everywhere.
            set_wrap_mode(renderer, &a, wrap);
            set_wrap_mode(renderer, &b, wrap);
            self.pads = Some((a, b));
            self.pad_alloc = pad_live;
            true
        } else {
            self.pads = None;
            false
        }
    }

    /// Age the pad pair, freeing it once this window has gone `keep_frames`
    /// without the per-window path. The next frame that needs it re-allocates
    /// through `ensure_pads`.
    fn age_pads(&mut self, keep_frames: u32) {
        self.pads_idle_frames = self.pads_idle_frames.saturating_add(1);
        if self.pads_idle_frames > keep_frames {
            self.pads = None;
        }
    }

    pub fn resize(&mut self, renderer: &mut GlesRenderer, alloc: Size<i32, Physical>, wrap: i32) {
        use smithay::backend::renderer::Offscreen;
        let buf_size = alloc.to_logical(1).to_buffer(1, Transform::Normal);
        if let Ok(t1) =
            Offscreen::<GlesTexture>::create_buffer(renderer, Fourcc::Abgr8888, buf_size)
            && let Ok(t3) =
                Offscreen::<GlesTexture>::create_buffer(renderer, Fourcc::Abgr8888, buf_size)
        {
            set_wrap_mode(renderer, &t1, wrap);
            set_wrap_mode(renderer, &t3, wrap);
            zero_texture(renderer, &t1, alloc);
            zero_texture(renderer, &t3, alloc);
            self.texture = t1;
            self.mask = t3;
            self.alloc = alloc;
            self.mask_stamp = None;
            self.zeroed = true;
            self.dirty = true;
            // Stored damage rects are at the old size — drop them; next render reseeds.
            self.damage_bag.reset();
        }
    }
}

/// Per-output shared blurred-background state: the ping-pong pair every
/// unoccluded window slices its frost out of, plus the refresh cadence. Keyed
/// per output in `RenderCache` — outputs differ in size and render on their own
/// vblanks, so one global entry would thrash (recreate + full re-blur on every
/// size mismatch) the moment a second output exists.
pub struct SharedBlur {
    /// Allocated on first use. Lazy because the cadence below also paces the
    /// windows that *don't* sample the texture: one that overlaps something
    /// lower renders its own backdrop, and a frame where every blurred window
    /// does that must not pay a full-output blur nobody reads.
    pub textures: Option<(GlesTexture, GlesTexture)>,
    /// The textures predate the view/content stamped below, because the beat
    /// ticked on a frame where nothing sampled them. Keeping them around
    /// unrendered — rather than dropping them — is what stops a window sliding
    /// in and out of occlusion from reallocating a full-output pair per frame.
    pub stale: bool,
    pub size: Size<i32, Physical>,
    pub refreshed_at: Option<std::time::Instant>,
    pub view: ViewStamp,
    pub background: Vec<(Id, CommitCounter)>,
    /// Backoff after a failed capture — see [`RenderBackoff`].
    pub backoff: RenderBackoff,
    /// Consecutive frames the backdrop has not been worth rendering. The
    /// decision is per frame; the allocation deliberately is not, so a window
    /// sliding in and out of occlusion doesn't free and re-allocate a
    /// full-output pair every frame. Starts unpaid: a lone window has to clear
    /// the high threshold to claim the shared path in the first place.
    pub unpaid_frames: u32,
}

impl SharedBlur {
    /// Cadence-only: no textures until a window actually samples the backdrop.
    fn new() -> Self {
        Self {
            textures: None,
            stale: true,
            size: Size::default(),
            refreshed_at: None,
            view: ViewStamp::never_captured(),
            background: Vec::new(),
            backoff: RenderBackoff::default(),
            unpaid_frames: 1,
        }
    }
}

/// Retry pacing for the shared backdrop's capture. A failure there is usually
/// persistent — a lost context, an exhausted GPU — and the retry drops and
/// re-allocates two output-sized textures, so retrying every frame turns one
/// broken renderer into a per-frame allocation storm.
#[derive(Default)]
pub struct RenderBackoff {
    consecutive_failures: u32,
    skip_frames: u32,
}

/// Cap on the doubling, so a renderer that comes back is picked up within a
/// couple of seconds rather than minutes.
const BACKOFF_MAX_SKIP: u32 = 64;

impl RenderBackoff {
    /// Whether this frame may attempt the capture, consuming one frame of any
    /// pending backoff.
    fn ready(&mut self) -> bool {
        if self.skip_frames == 0 {
            return true;
        }
        self.skip_frames -= 1;
        false
    }

    fn note_failure(&mut self) {
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        self.skip_frames = (1u32 << self.consecutive_failures.min(6)).min(BACKOFF_MAX_SKIP);
    }

    fn note_success(&mut self) {
        self.consecutive_failures = 0;
        self.skip_frames = 0;
    }
}

/// Frames the shared textures survive without being worth rendering. Long
/// enough to cover a drag or an animation moving a window across another,
/// short enough that switching to a single small frosted window gives the two
/// output-sized allocations back promptly.
const SHARED_KEEP_FRAMES: u32 = 60;

/// Frames a window's padded ping-pong pair survives without the per-window
/// path. Same reasoning as [`SHARED_KEEP_FRAMES`] from the other side: the pads
/// are the largest per-window allocation, and a window that settles onto the
/// shared slice — or simply stops changing — would otherwise hold them for its
/// whole life.
const PAD_KEEP_FRAMES: u32 = 60;

/// Output coverage the windows slicing the shared backdrop have to reach
/// before it is claimed, and the coverage it is given up below. The claim
/// threshold is under 1.0 because the shared render walks only the background
/// element slice while a per-window render walks everything below its window;
/// the release threshold is far enough below it that a zoom or a resize drag
/// crossing the claim point flips the path once, not once per frame.
const SHARED_CLAIM_COVERAGE: f64 = 0.8;
const SHARED_RELEASE_COVERAGE: f64 = 0.5;

/// What the shared backdrop's payoff is judged on.
struct SharedPayoff {
    /// Windows that would slice the shared backdrop instead of rendering their
    /// own — the occluded ones never read it and are not counted.
    slicers: usize,
    /// Output area those windows' own backdrop renders would cover between
    /// them, as a fraction of one full-output render.
    coverage: f64,
    /// Nothing is left in the background bucket to capture — a visually
    /// fullscreen output, where the shared render would blur the clear colour
    /// into an opaque black backdrop and any overlay-layer frost would slice
    /// that.
    background_empty: bool,
    was_paying: bool,
}

/// Whether the shared backdrop earns its full-output render this frame.
///
/// One full-output scene render plus one full-output Kawase is a fixed cost
/// that stops the per-window path from scaling with window count — but it is
/// only ever a saving against *several* windows' own backdrops. A lone window
/// pays that full-output cost to replace a render of its own padded rect, which
/// is at most the same and usually far less, however much of the output it
/// covers; coverage on top is what keeps two small widgets from claiming a
/// full-output pass between them.
fn shared_backdrop_pays(input: &SharedPayoff) -> bool {
    if input.slicers < 2 || input.background_empty {
        return false;
    }
    let threshold = if input.was_paying {
        SHARED_RELEASE_COVERAGE
    } else {
        SHARED_CLAIM_COVERAGE
    };
    input.coverage >= threshold
}

/// What the shared backdrop's refresh decision is made from.
struct SharedRefreshInputs {
    /// Time since the last refresh; `None` when the backdrop has never been
    /// blurred at all.
    since_refresh: Option<std::time::Duration>,
    /// Camera position or zoom moved. Capture and sample then have to happen at
    /// the same view, or every window slices its frost out of a backdrop that no
    /// longer lines up with what is on screen.
    view_changed: bool,
    /// The set of background elements changed — see
    /// [`background_structure_changed`].
    bg_structure_changed: bool,
    /// A background element committed new content since the last refresh.
    bg_content_changed: bool,
    animate_blur_fps: u32,
}

/// The view and structure terms are deliberately uncapped: throttling either
/// makes staleness representable — the backdrop lags the camera, or shows a
/// background that is no longer on screen, while every window keeps slicing it.
/// Only the background's own animation rides `animate_blur_fps`.
fn shared_refresh_due(input: &SharedRefreshInputs) -> bool {
    let Some(since_refresh) = input.since_refresh else {
        return true;
    };
    if input.view_changed || input.bg_structure_changed {
        return true;
    }
    // `0` means "never re-sample the background's own animation" — the frost is
    // captured once and thereafter only follows the view. Returning here also
    // keeps `1.0 / fps` from becoming an infinite `Duration`, which panics.
    if input.animate_blur_fps == 0 || !input.bg_content_changed {
        return false;
    }
    since_refresh >= std::time::Duration::from_secs_f64(1.0 / input.animate_blur_fps as f64)
}

/// Where the content *behind* one window begins: below its own trailing chrome.
///
/// `behind_start` points at that chrome, and it is wrong on both counts. The
/// shadow's geometry strictly contains the window rect, so the occlusion probe
/// would report every shadowed window as occluded; and capturing from there
/// puts the window's own shadow in its own frost, which the shared backdrop
/// cannot do — so the two paths would show different frost, and every flip
/// between them would pop.
fn behind_own_chrome(
    behind_start: usize,
    trailing_chrome: usize,
    background_start: usize,
) -> usize {
    (behind_start + trailing_chrome).min(background_start)
}

/// Padding around the blur crop so the Kawase reach never touches a texture
/// edge: window-edge samples must see real backdrop, not clamped border
/// pixels. Sized to the blur's worst-case reach at the deepest mip.
fn blur_pad(strength: f32, passes: usize) -> i32 {
    ((strength * (1u32 << (passes + 1)) as f32).ceil() as i32).clamp(16, 128)
}

/// Step the per-window blur allocations are rounded up to, in physical pixels.
/// Sizing them to the exact on-screen extent reallocated four textures per
/// blurred window per frame of a zoom; a step turns that into one realloc per
/// step crossed, and leaves the small extent changes an easing tail is made of
/// costing nothing at all. Waste is bounded by the step rather than by the zoom
/// factor, and the blur still runs over the live sub-rect only, so cost within
/// a step stays proportional to what is on screen.
const CACHE_STEP: i32 = 64;

/// Quantize one on-screen extent: grow immediately, give the step back only
/// half a step past the point where it would be taken again.
///
/// Without that band an extent sitting on a step boundary — where a slow zoom
/// or a resize drag parks it — reallocates in both directions on alternate
/// frames, which is the cost this quantization exists to remove.
fn quantized_extent(live: i32, current: i32) -> i32 {
    let step = CACHE_STEP;
    let wanted = (live.max(1) as u32).div_ceil(step as u32) as i32 * step;
    if wanted > current {
        return wanted;
    }
    // `current - step` is what the next step down covers, so a live extent that
    // has fallen half a step clear of it can give the current one up without
    // landing right back on the growth threshold.
    if live <= current - step - step / 2 {
        return wanted;
    }
    current
}

fn quantized_alloc(live: Size<i32, Physical>, current: Size<i32, Physical>) -> Size<i32, Physical> {
    (
        quantized_extent(live.w, current.w),
        quantized_extent(live.h, current.h),
    )
        .into()
}

/// Wrap mode for the backdrop textures the padded crop samples past.
/// MIRRORED_REPEAT reflects real backdrop back across the boundary; plain CLAMP
/// streaks the edge row/column and the default REPEAT wraps in the opposite
/// side. MIRRORED_REPEAT on an NPOT texture needs GLES 3, so GLES 2 falls back
/// to CLAMP — streaks, but never an incomplete texture's all-black sample.
fn backdrop_wrap_mode(renderer: &mut GlesRenderer) -> i32 {
    use smithay::backend::renderer::gles::ffi;
    renderer
        .with_context(|gl| unsafe {
            let gles3 = std::ffi::CStr::from_ptr(gl.GetString(ffi::VERSION) as *const _)
                .to_string_lossy()
                .strip_prefix("OpenGL ES ")
                .and_then(|s| s.chars().next())
                .and_then(|c| c.to_digit(10))
                .is_some_and(|major| major >= 3);
            if gles3 {
                ffi::MIRRORED_REPEAT
            } else {
                ffi::CLAMP_TO_EDGE
            }
        })
        .unwrap_or(ffi::CLAMP_TO_EDGE) as i32
}

/// Clear a freshly allocated texture end to end.
///
/// Only the live sub-rect of a quantized allocation is ever rendered into, and
/// the slack past it comes out of `create_buffer` undefined. The on-screen
/// element's `src` names the live extent, but at a fractional output scale the
/// destination does not invert back to it exactly, so the filter at the far
/// edge can still reach the first slack texel — transparent black is a far
/// better thing for it to find than whatever the driver handed over.
fn zero_texture(renderer: &mut GlesRenderer, texture: &GlesTexture, size: Size<i32, Physical>) {
    use smithay::backend::renderer::{Bind, Color32F, Frame, Renderer};
    let mut texture = texture.clone();
    let Ok(mut target) = renderer.bind(&mut texture) else {
        return;
    };
    if let Ok(mut frame) = renderer.render(&mut target, size, Transform::Normal) {
        let _ = frame.clear(Color32F::TRANSPARENT, &[Rectangle::from_size(size)]);
        let _ = frame.finish();
    }
}

/// Wrap mode has to be re-applied at every allocation: `create_buffer` leaves
/// the GL default, and the caches drop their textures on resize.
fn set_wrap_mode(renderer: &mut GlesRenderer, texture: &GlesTexture, wrap: i32) {
    use smithay::backend::renderer::gles::ffi;
    let id = texture.tex_id();
    let _ = renderer.with_context(|gl| unsafe {
        gl.BindTexture(ffi::TEXTURE_2D, id);
        gl.TexParameteri(ffi::TEXTURE_2D, ffi::TEXTURE_WRAP_S, wrap);
        gl.TexParameteri(ffi::TEXTURE_2D, ffi::TEXTURE_WRAP_T, wrap);
        gl.BindTexture(ffi::TEXTURE_2D, 0);
    });
}

/// Where to capture the backdrop for one window's padded rect.
struct BackdropCapture {
    /// The part of the padded rect that has real backdrop behind it. Capturing
    /// only this is what keeps the render window-sized instead of output-sized.
    clipped: Rectangle<i32, Physical>,
    /// The padded rect's origin in the captured texture's own coordinates.
    /// Negative on any side that overhangs the output: there the crop samples
    /// past the texture, and the mirror wrap reflects real backdrop back across
    /// the output edge instead of leaving a ring the blur bleeds inward.
    src_loc: Point<i32, Physical>,
    /// The padded rect fits on the output, so the capture can render straight
    /// into the pad texture — no scratch, no crop.
    direct: bool,
}

fn backdrop_capture(
    padded: Rectangle<i32, Physical>,
    output_size: Size<i32, Physical>,
) -> Option<BackdropCapture> {
    let clipped = padded.intersection(Rectangle::from_size(output_size))?;
    Some(BackdropCapture {
        clipped,
        src_loc: padded.loc - clipped.loc,
        direct: clipped == padded,
    })
}

/// Frames a scratch texture survives without being asked for again. An extent
/// recurs for as long as a window sits at an output edge — an anchored layer
/// surface, a filled or maximized window, an edge-snapped one — which is the
/// case this pool exists for; it stops recurring the moment the window moves.
const SCRATCH_KEEP_FRAMES: u32 = 60;

/// What the pool may hold, as a multiple of one output-sized texture. Bounding
/// by entry count instead would have to assume the worst entry size: a filled
/// window's scratch is nearly output-sized (33 MB at 4K) while an anchored
/// bar's is a fraction of that.
const SCRATCH_BUDGET_OUTPUTS: usize = 2;

struct ScratchEntry<T> {
    size: Size<i32, Physical>,
    texture: T,
    idle_frames: u32,
}

/// Per-output pool of the scratch textures the edge capture renders into.
///
/// Keyed on the extent and never quantized: the mirror wrap has to sit exactly
/// on the output boundary, so a texture of any other size would move the mirror
/// axis off the content edge and reflect clear colour back into the frost. That
/// rules out one reusable per-output scratch — hence a pool, which works
/// because the extents repeat frame to frame for exactly the windows that hit
/// this path: the ones parked against an output edge.
pub struct ScratchPool<T> {
    entries: Vec<ScratchEntry<T>>,
}

pub type BlurScratchPool = ScratchPool<GlesTexture>;

impl<T> Default for ScratchPool<T> {
    fn default() -> Self {
        Self {
            entries: Vec::new(),
        }
    }
}

fn scratch_bytes(size: Size<i32, Physical>) -> usize {
    (size.w.max(0) as usize) * (size.h.max(0) as usize) * 4
}

impl<T: Clone> ScratchPool<T> {
    /// The pooled texture for `size`, if there is one, marked as used.
    fn hit(&mut self, size: Size<i32, Physical>) -> Option<T> {
        let entry = self.entries.iter_mut().find(|e| e.size == size)?;
        entry.idle_frames = 0;
        Some(entry.texture.clone())
    }

    fn held_bytes(&self) -> usize {
        self.entries.iter().map(|e| scratch_bytes(e.size)).sum()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Drop least-recently-used entries until one more texture of `size` fits
    /// inside `budget_bytes`. Called *before* the allocation, so the budget
    /// bounds peak residency and not just what is held afterwards.
    fn make_room(&mut self, size: Size<i32, Physical>, budget_bytes: usize) {
        let wanted = scratch_bytes(size);
        while !self.entries.is_empty() && self.held_bytes() + wanted > budget_bytes {
            let lru = self
                .entries
                .iter()
                .enumerate()
                .max_by_key(|(_, e)| e.idle_frames)
                .map(|(i, _)| i)
                .expect("entries is non-empty");
            self.entries.remove(lru);
        }
    }

    /// Take ownership of a freshly created texture.
    fn store(&mut self, size: Size<i32, Physical>, texture: T) {
        self.entries.push(ScratchEntry {
            size,
            texture,
            idle_frames: 0,
        });
    }

    /// Age everything, and drop what has gone unused for too long. Entries used
    /// this frame come in at zero, so they survive `SCRATCH_KEEP_FRAMES` idle
    /// frames from here.
    fn end_frame(&mut self) {
        self.entries.retain_mut(|e| {
            e.idle_frames = e.idle_frames.saturating_add(1);
            e.idle_frames <= SCRATCH_KEEP_FRAMES
        });
    }
}

impl BlurScratchPool {
    /// The scratch texture for `size`, allocating one if the pool has none.
    ///
    /// Hands out a clone: `GlesTexture` is a handle, so the caller renders into
    /// the same texture the pool keeps for the next frame.
    fn acquire(
        &mut self,
        renderer: &mut GlesRenderer,
        size: Size<i32, Physical>,
        wrap: i32,
        budget_bytes: usize,
    ) -> Option<GlesTexture> {
        use smithay::backend::renderer::Offscreen;
        if let Some(texture) = self.hit(size) {
            return Some(texture);
        }
        self.make_room(size, budget_bytes);
        let buf_size = size.to_logical(1).to_buffer(1, Transform::Normal);
        let texture =
            Offscreen::<GlesTexture>::create_buffer(renderer, Fourcc::Abgr8888, buf_size).ok()?;
        set_wrap_mode(renderer, &texture, wrap);
        self.store(size, texture.clone());
        Some(texture)
    }
}

static BLUR_MASK_SRC: &str = include_str!("../shaders/blur_mask.glsl");

pub(crate) fn compile_blur_shaders(
    renderer: &mut GlesRenderer,
) -> (
    Option<GlesTexProgram>,
    Option<GlesTexProgram>,
    Option<GlesTexProgram>,
) {
    let uniforms = &[
        UniformName::new("u_halfpixel", UniformType::_2f),
        UniformName::new("u_offset", UniformType::_1f),
    ];
    match (
        renderer.compile_custom_texture_shader(BLUR_DOWN_SRC, uniforms),
        renderer.compile_custom_texture_shader(BLUR_UP_SRC, uniforms),
        renderer.compile_custom_texture_shader(BLUR_MASK_SRC, &[]),
    ) {
        (Ok(d), Ok(u), Ok(m)) => (Some(d), Some(u), Some(m)),
        (Err(e), _, _) | (_, Err(e), _) | (_, _, Err(e)) => {
            tracing::error!("Failed to compile blur shaders: {e:?}");
            (None, None, None)
        }
    }
}

/// Run dual Kawase blur passes (downscale then upscale) between two textures.
/// After completion, `tex_a` contains the blurred result.
///
/// The mip chain is derived from the textures' own extent, so the passes always
/// cover every texel of them. Running over a sub-rect of a larger allocation
/// instead leaves the remainder as `create_buffer` handed it over — undefined —
/// and the taps at the sub-rect's edge read straight into it, because the
/// mirror wrap reflects about the allocation edge and not the sub-rect's.
fn render_blur(
    renderer: &mut GlesRenderer,
    down_shader: &GlesTexProgram,
    up_shader: &GlesTexProgram,
    tex_a: &mut GlesTexture,
    tex_b: &mut GlesTexture,
    offset: f32,
    passes: usize,
) -> Result<(), GlesError> {
    use smithay::backend::renderer::Texture;

    let alloc = tex_a.size();

    for i in 0..passes {
        blur_pass(
            renderer,
            down_shader,
            tex_a,
            tex_b,
            alloc,
            offset,
            i,
            passes,
            true,
        )?;
        std::mem::swap(tex_a, tex_b);
    }

    for i in 0..passes {
        blur_pass(
            renderer, up_shader, tex_a, tex_b, alloc, offset, i, passes, false,
        )?;
        std::mem::swap(tex_a, tex_b);
    }

    // 2*passes swaps (even) → tex_a has the result
    Ok(())
}

/// Single blur pass: render src (tex_a) into target (tex_b) with the given shader.
#[allow(clippy::too_many_arguments)]
fn blur_pass(
    renderer: &mut GlesRenderer,
    shader: &GlesTexProgram,
    tex_a: &GlesTexture,
    tex_b: &mut GlesTexture,
    alloc: Size<i32, smithay::utils::Buffer>,
    offset: f32,
    i: usize,
    passes: usize,
    downscale: bool,
) -> Result<(), GlesError> {
    use smithay::backend::renderer::{Bind, Color32F, Frame, Renderer};

    let (src_shift, dst_shift) = if downscale {
        (i, i + 1)
    } else {
        (passes - i, passes - i - 1)
    };

    let src_w = (alloc.w >> src_shift).max(1);
    let src_h = (alloc.h >> src_shift).max(1);
    let dst_w = (alloc.w >> dst_shift).max(1);
    let dst_h = (alloc.h >> dst_shift).max(1);

    // Standard Kawase: the tap distance is a UV offset, and UV is normalized
    // over the whole texture, so one texel of this mip is `1 / (alloc >> shift)`.
    let half_pixel = if downscale {
        [1.0 / src_w as f32, 1.0 / src_h as f32]
    } else {
        [0.5 / src_w as f32, 0.5 / src_h as f32]
    };
    let pass_offset = offset / (1 << src_shift) as f32;

    let dst_phys: Size<i32, Physical> = (dst_w, dst_h).into();
    let src_buf: Rectangle<f64, smithay::utils::Buffer> =
        Rectangle::from_size((src_w as f64, src_h as f64).into());

    let src = tex_a.clone();
    {
        let mut target = renderer.bind(tex_b)?;
        let mut frame = renderer.render(&mut target, dst_phys, Transform::Normal)?;
        frame.clear(Color32F::TRANSPARENT, &[Rectangle::from_size(dst_phys)])?;
        frame.render_texture_from_to(
            &src,
            src_buf,
            Rectangle::from_size(dst_phys),
            &[Rectangle::from_size(dst_phys)],
            &[],
            Transform::Normal,
            1.0,
            Some(shader),
            &[
                Uniform::new("u_halfpixel", half_pixel),
                Uniform::new("u_offset", pass_offset),
            ],
        )?;
        let _ = frame.finish()?;
    }
    Ok(())
}

/// Which element group a blur request belongs to — determines its prefix offset.
#[derive(Clone, Copy)]
pub(crate) enum BlurLayer {
    Overlay,
    Top,
    Pinned,
    Normal,
    Widget,
}

/// Data extracted from a blur request.
pub(crate) struct BlurRequestData {
    pub surface_id: smithay::reexports::wayland_server::backend::ObjectId,
    pub screen_rect: Rectangle<i32, Physical>,
    pub elem_start: usize,
    pub elem_count: usize,
    /// Elements this window pushed *below* its blur insertion point — its own
    /// shadow, and a layer surface's border too. Counted at the push site, not
    /// derived from the config: chrome that failed to get a shader pushes
    /// nothing, and a derived count would silently walk off the window.
    pub trailing_chrome: usize,
    pub layer: BlurLayer,
    /// Client-requested blur region in mask-local physical coords (origin at
    /// `screen_rect.loc`). `None` = whole-window blur (no client region set).
    /// Empty list = client opted out (handled at the trigger site, never
    /// constructed here).
    pub region_rects: Option<Arc<Vec<Rectangle<i32, Physical>>>>,
}

/// Parts of the window-sized mask that have to be zeroed back out so only a
/// client-requested blur region keeps frost. `None` means no client region, so
/// the whole window stays frosted and nothing is subtracted.
fn mask_region_complement(
    win_size: Size<i32, Physical>,
    region_rects: Option<&[Rectangle<i32, Physical>]>,
) -> Vec<Rectangle<i32, Physical>> {
    match region_rects {
        Some(rects) => {
            Rectangle::subtract_rects_many([Rectangle::from_size(win_size)], rects.iter().copied())
        }
        None => Vec::new(),
    }
}

/// Process blur requests: for each blurred window, render behind-content to FBO,
/// crop the window region, run Kawase blur passes, and insert the result.
#[allow(clippy::too_many_arguments)]
pub(crate) fn process_blur_requests(
    state: &mut crate::state::DriftWm,
    renderer: &mut GlesRenderer,
    output: &Output,
    output_scale: f64,
    camera: Point<f64, Logical>,
    zoom: f64,
    all_elements: &mut Vec<OutputRenderElements>,
    blur_requests: &[BlurRequestData],
    overlay_prefix: usize,
    top_prefix: usize,
    pinned_prefix: usize,
    normal_prefix: usize,
    widget_prefix: usize,
    background_start: usize,
) {
    use smithay::backend::renderer::Color32F;
    use smithay::backend::renderer::damage::OutputDamageTracker;
    use smithay::backend::renderer::{Bind, Frame, Offscreen, Renderer};

    let logical_size = crate::state::output_logical_size(output);
    let output_size: Size<i32, Physical> = logical_size.to_physical_precise_round(output_scale);
    let out_buf_size = output_size.to_logical(1).to_buffer(1, Transform::Normal);
    // Resolved once per renderer: the query behind it is a `glGetString` plus a
    // string walk, and the answer is a property of the GL context.
    let wrap = match state.render.blur_wrap_mode {
        Some(wrap) => wrap,
        None => *state
            .render
            .blur_wrap_mode
            .insert(backdrop_wrap_mode(renderer)),
    };

    let down_shader = state.render.blur_down_shader.clone().unwrap();
    let up_shader = state.render.blur_up_shader.clone().unwrap();
    let blur_passes = state.config.effects.blur_radius as usize;
    let blur_strength = state.config.effects.blur_strength as f32;
    let context_id = renderer.context_id();
    let output_name = output.name();
    let geom_gen = state.render.blur_geometry_generation;
    let view = ViewStamp { camera, zoom };

    // Precompute per-request behind depth (index into all_elements where "below this window" begins)
    let behind_starts: Vec<usize> = blur_requests
        .iter()
        .map(|req| {
            let prefix = match req.layer {
                BlurLayer::Overlay => overlay_prefix,
                BlurLayer::Top => top_prefix,
                BlurLayer::Pinned => pinned_prefix,
                BlurLayer::Normal => normal_prefix,
                BlurLayer::Widget => widget_prefix,
            };
            (prefix + req.elem_start + req.elem_count).min(all_elements.len())
        })
        .collect();

    let pad = blur_pad(blur_strength, blur_passes);

    // Everything below the window's own chrome: what the occlusion probe scans,
    // and what the per-window capture renders.
    let backdrop_starts: Vec<usize> = blur_requests
        .iter()
        .enumerate()
        .map(|(i, req)| behind_own_chrome(behind_starts[i], req.trailing_chrome, background_start))
        .collect();

    // behind_starts alone is a z-order test: side-by-side windows all read as
    // "stacked" and lose the shared slice. Only what reaches into the padded
    // capture counts — for the fall-through and for the fingerprint alike,
    // which is why one walk produces both.
    let elem_scale = smithay::utils::Scale::from(output_scale);
    let fingerprints: Vec<BackdropFingerprint> = blur_requests
        .iter()
        .enumerate()
        .map(|(i, req)| {
            backdrop_fingerprint(
                all_elements[backdrop_starts[i]..background_start]
                    .iter()
                    .map(|e| (e.id(), e.current_commit(), e.geometry(elem_scale))),
                all_elements[background_start..]
                    .iter()
                    .map(|e| (e.id(), e.current_commit())),
                req.screen_rect,
                pad,
                req.region_rects.as_deref().map(|v| v.as_slice()),
            )
        })
        .collect();

    // What the windows that would slice the shared backdrop would otherwise
    // render for themselves: the same clipped padded rects the per-window path
    // below captures. Their total against one full-output render is what
    // decides whether the shared backdrop is worth having this frame.
    let mut slicers = 0usize;
    let mut slicer_area = 0f64;
    for (i, req) in blur_requests.iter().enumerate() {
        if fingerprints[i].occluded_by_lower
            || req.screen_rect.size.w <= 0
            || req.screen_rect.size.h <= 0
        {
            continue;
        }
        let padded = padded_rect(req.screen_rect, pad);
        // A window entirely off the output has no backdrop to capture and never
        // reaches either path, so it is not a slicer and contributes no area —
        // the count and the area have to agree about that, or two off-screen
        // windows would clear the count gate with nothing behind it.
        if let Some(capture) = backdrop_capture(padded, output_size) {
            slicers += 1;
            slicer_area += capture.clipped.size.w as f64 * capture.clipped.size.h as f64;
        }
    }
    let output_area = output_size.w as f64 * output_size.h as f64;

    // The scene background is blurred ONCE into a shared full-output texture and
    // each unoccluded window slices its rect out of it, so cost stops scaling
    // with the number of blurred windows. Trade-off: a window overlapping
    // another window frosts only the background beneath, so it renders its own
    // backdrop instead — but still follows the cadence decided here.
    let background_slice = &all_elements[background_start.min(all_elements.len())..];
    let mut backdrop_beat = false;
    let shared_slice_ok;
    {
        let mut shared = state
            .render
            .shared_blur
            .remove(&output_name)
            .unwrap_or_else(SharedBlur::new);
        let background = background_signature(background_slice);
        let due = shared_refresh_due(&SharedRefreshInputs {
            since_refresh: shared.refreshed_at.map(|at| at.elapsed()),
            view_changed: shared.view != view,
            bg_structure_changed: background_structure_changed(&shared.background, &background),
            bg_content_changed: shared.background != background,
            animate_blur_fps: state.config.effects.animate_blur_fps,
        });
        // The beat ticks whether or not the textures are rendered from it: it is
        // what keeps a window that renders its own backdrop — occluded, or on
        // the per-window path because the shared one doesn't pay — animating
        // over an animated background.
        if due {
            shared.refreshed_at = Some(std::time::Instant::now());
            shared.view = view;
            shared.background = background;
            shared.stale = true;
            backdrop_beat = true;
        }
        if shared.size != output_size {
            shared.textures = None;
            shared.size = output_size;
        }
        let pays = shared_backdrop_pays(&SharedPayoff {
            slicers,
            coverage: if output_area > 0.0 {
                slicer_area / output_area
            } else {
                0.0
            },
            background_empty: background_slice.is_empty(),
            was_paying: shared.unpaid_frames == 0,
        });
        if pays {
            shared.unpaid_frames = 0;
        } else {
            shared.unpaid_frames = shared.unpaid_frames.saturating_add(1);
            if shared.unpaid_frames > SHARED_KEEP_FRAMES {
                shared.textures = None;
            }
        }
        if pays && (shared.stale || shared.textures.is_none()) && shared.backoff.ready() {
            if shared.textures.is_none() {
                let a = Offscreen::<GlesTexture>::create_buffer(
                    renderer,
                    Fourcc::Abgr8888,
                    out_buf_size,
                );
                let b = Offscreen::<GlesTexture>::create_buffer(
                    renderer,
                    Fourcc::Abgr8888,
                    out_buf_size,
                );
                if let (Ok(a), Ok(b)) = (a, b) {
                    // The Kawase passes sample past these edges at every mip; an
                    // NPOT texture left at the GL default REPEAT is incomplete on
                    // GLES 2 and samples black everywhere.
                    set_wrap_mode(renderer, &a, wrap);
                    set_wrap_mode(renderer, &b, wrap);
                    shared.textures = Some((a, b));
                }
            }
            match shared.textures.as_mut() {
                Some((tex_a, tex_b)) => {
                    let mut rendered = false;
                    if let Ok(mut target) = renderer.bind(&mut *tex_a) {
                        let mut dt =
                            OutputDamageTracker::new(output_size, output_scale, Transform::Normal);
                        rendered = dt
                            .render_output(
                                renderer,
                                &mut target,
                                0,
                                background_slice,
                                [0.0f32, 0.0, 0.0, 1.0],
                            )
                            .is_ok();
                    }
                    let blurred = rendered
                        && render_blur(
                            renderer,
                            &down_shader,
                            &up_shader,
                            tex_a,
                            tex_b,
                            blur_strength * output_scale as f32,
                            blur_passes,
                        )
                        .is_ok();
                    if blurred {
                        shared.stale = false;
                        shared.backoff.note_success();
                    } else {
                        // Half-written: drop it rather than let windows slice frost
                        // out of a texture whose contents are unknown.
                        shared.textures = None;
                        shared.backoff.note_failure();
                    }
                }
                // The allocation itself failed.
                None => shared.backoff.note_failure(),
            }
        }
        // Textures the payoff rule left unrendered still hold the last view's
        // backdrop, so they may only be sliced on a frame that paid for them.
        shared_slice_ok = pays && !shared.stale && shared.textures.is_some();
        state.render.shared_blur.insert(output_name.clone(), shared);
    }

    let mut needs_recompute: Vec<bool> = Vec::with_capacity(blur_requests.len());
    let mut mask_forced: Vec<bool> = Vec::with_capacity(blur_requests.len());
    for (i, req) in blur_requests.iter().enumerate() {
        let win_size = req.screen_rect.size;
        if win_size.w <= 0 || win_size.h <= 0 {
            needs_recompute.push(false);
            mask_forced.push(false);
            continue;
        }
        let key = (output_name.clone(), req.surface_id.clone());
        if !state.render.blur_cache.contains_key(&key) {
            let alloc = quantized_alloc(win_size, Size::default());
            if let Some(c) = BlurCache::new(renderer, alloc, wrap) {
                state.render.blur_cache.insert(key.clone(), c);
            } else {
                needs_recompute.push(false);
                mask_forced.push(false);
                continue;
            }
        }
        let cache = state.render.blur_cache.get_mut(&key).unwrap();
        let alloc = quantized_alloc(win_size, cache.alloc);
        if cache.alloc != alloc {
            cache.resize(renderer, alloc, wrap);
        }
        // Reset by the per-window path below on the frames that take it.
        cache.age_pads(PAD_KEEP_FRAMES);

        let backdrop_hash = fingerprints[i].hash;
        let backdrop_changed = cache.last_backdrop_hash != backdrop_hash;
        let geom_changed = cache.last_geometry_generation != geom_gen;
        // No layer is exempt from a view change: the capture is taken in screen
        // space, and zoom redraws the background itself since a shader may
        // consume `u_zoom`. A screen-fixed layer additionally has the whole
        // canvas panning underneath it. The beat covers all of that today —
        // `shared_refresh_due` is uncapped on the view — and this stays as the
        // window's own record of what it was computed at, so a narrower beat
        // could never leave one window behind the view it was captured from.
        let view_dirty = cache.last_view != view;

        // Occluded windows follow the cadence too, even though they re-render
        // their own backdrop to do it: their frost is over the same background,
        // and holding them back is what leaves it frozen while its neighbours
        // animate.
        if backdrop_changed || geom_changed || view_dirty || backdrop_beat {
            cache.dirty = true;
        }
        mask_forced.push(cache.force_dirty_frames > 0);
        if cache.force_dirty_frames > 0 {
            cache.dirty = true;
            cache.force_dirty_frames -= 1;
        }
        cache.last_backdrop_hash = backdrop_hash;
        cache.last_geometry_generation = geom_gen;
        cache.last_view = view;

        needs_recompute.push(cache.dirty);
    }

    let mask_shader = state.render.blur_mask_shader.clone();
    let scratch_budget = SCRATCH_BUDGET_OUTPUTS * scratch_bytes(output_size);

    // Whether loop 1 actually rebuilt the texture. Its bail-outs are routine —
    // a window fully off the output has no backdrop to capture — and loop 2
    // must not multiply the mask into a texture that still carries the previous
    // one, nor clear `dirty` on a rebuild that never happened.
    let mut rebuilt = vec![false; blur_requests.len()];

    // Capture the backdrop behind each dirty window, crop it, and blur it.
    for (i, req) in blur_requests.iter().enumerate() {
        if !needs_recompute[i] {
            continue;
        }
        let win_size = req.screen_rect.size;
        if win_size.w <= 0 || win_size.h <= 0 {
            continue;
        }
        let key = (output_name.clone(), req.surface_id.clone());
        let Some(cache) = state.render.blur_cache.get_mut(&key) else {
            continue;
        };

        // The shared slice is only exact when nothing but scene background
        // lies beneath this window; a window that actually overlaps a lower
        // one falls through to the per-window path (on the same cadence), so
        // lower windows show in its frost. A backdrop that wasn't worth
        // rendering, and missing shared textures (GL alloc failure), also fall
        // through — skipping would insert this window's never-rendered texture
        // as an invisible blur.
        if !fingerprints[i].occluded_by_lower
            && shared_slice_ok
            && let Some(shared) = state.render.shared_blur.get(&output_name)
            && let Some((tex_a, _)) = shared.textures.as_ref()
        {
            // Slice this window's rect out of the shared blurred background.
            // Already blurred full-screen, so edges see real neighbours and
            // no padding is needed.
            let shared_src = tex_a.clone();
            let Ok(mut target) = renderer.bind(&mut cache.texture) else {
                continue;
            };
            let Ok(mut frame) = renderer.render(&mut target, win_size, Transform::Normal) else {
                continue;
            };
            let _ = frame.clear(Color32F::TRANSPARENT, &[Rectangle::from_size(win_size)]);
            let src_rect: Rectangle<f64, smithay::utils::Buffer> = Rectangle::new(
                (req.screen_rect.loc.x as f64, req.screen_rect.loc.y as f64).into(),
                (win_size.w as f64, win_size.h as f64).into(),
            );
            let sliced = frame
                .render_texture_from_to(
                    &shared_src,
                    src_rect,
                    Rectangle::from_size(win_size),
                    &[Rectangle::from_size(win_size)],
                    &[],
                    Transform::Normal,
                    1.0,
                    None,
                    &[],
                )
                .is_ok();
            let _ = frame.finish();
            rebuilt[i] = sliced;
            continue;
        }

        // Capture WITH padding: blur samples past the window edge must see real
        // backdrop, not clamped border pixels (the edge-fade bevel).
        let pad_live = pad_extent(win_size, pad);
        let padded = padded_rect(req.screen_rect, pad);
        // Entirely off the output: no backdrop exists to capture, and the
        // cached texture is not visible anyway.
        let Some(capture) = backdrop_capture(padded, output_size) else {
            continue;
        };

        // The damage tracker walks the whole element slice whatever the target
        // size, and the per-window offset rules out the shared-depth reuse an
        // output-sized capture allowed — so trim the slice to the elements that
        // can actually land in this capture before wrapping them.
        let relocated: Vec<RelocateRenderElement<&OutputRenderElements>> = all_elements
            [backdrop_starts[i]..]
            .iter()
            .filter(|e| e.geometry(elem_scale).overlaps(capture.clipped))
            .map(|e| {
                RelocateRenderElement::from_element(
                    e,
                    (-capture.clipped.loc.x, -capture.clipped.loc.y),
                    Relocate::Relative,
                )
            })
            .collect();

        // Nothing survives beneath this window to sample. The live case is a
        // fullscreen window that conceals the canvas (an output whose wallpaper
        // has not been cached yet is the degenerate one): the cull leaves it
        // bottom-most on the output, so the capture would be the probe's own
        // opaque-black clear, blurred into a frosted black slab and masked to
        // the window's alpha — the scene behind it read as solid black. A
        // translucent fullscreen window conceals nothing, so the canvas stays
        // drawn beneath it and it finds real content here. Zero instead, so the
        // window falls through to whatever is really behind, and settle rather
        // than retry: a slice that later has content hashes differently, and
        // leaving fullscreen moves the camera, so either one dirties the cache
        // again.
        // Before `ensure_pads`, so the padded pair is not allocated for a
        // capture that will not happen.
        if relocated.is_empty() {
            zero_texture(renderer, &cache.texture, cache.alloc);
            // The splice below drops the element while this holds, and an
            // element leaving the frame is what damages what it vacated — so
            // the frost the texture used to hold does not survive on screen.
            cache.zeroed = true;
            cache.dirty = false;
            continue;
        }

        if !cache.ensure_pads(renderer, pad_live, wrap) {
            continue;
        }
        cache.pads_idle_frames = 0;
        let Some((pad_a, pad_b)) = cache.pads.as_mut() else {
            continue;
        };

        if capture.direct {
            let Ok(mut target) = renderer.bind(&mut *pad_a) else {
                continue;
            };
            let mut dt = OutputDamageTracker::new(pad_live, output_scale, Transform::Normal);
            // A failed capture leaves the pad holding the previous frame — blur
            // and cache it and the window keeps frost from wherever it used to
            // be, with nothing left marking it stale.
            if dt
                .render_output(
                    renderer,
                    &mut target,
                    0,
                    &relocated,
                    [0.0f32, 0.0, 0.0, 1.0],
                )
                .is_err()
            {
                continue;
            }
        } else {
            // The padded rect overhangs the output, so the capture goes into a
            // texture sized to exactly the part that has real backdrop and the
            // crop below samples past it. The mirror must be about the boundary
            // where content ends — the output edge — which is why it sits on
            // this exactly-clipped texture and not on the pad texture, whose own
            // edge is up to `pad` further out with nothing but clear colour in
            // between.
            //
            // Pooled by extent rather than allocated per frame: a window parked
            // against an edge — an anchored bar, a filled window — asks for the
            // same extent on every dirty frame, which during a pan is every
            // frame.
            let pool = state
                .render
                .blur_scratch
                .entry(output_name.clone())
                .or_default();
            let Some(mut scratch) =
                pool.acquire(renderer, capture.clipped.size, wrap, scratch_budget)
            else {
                continue;
            };
            {
                let Ok(mut target) = renderer.bind(&mut scratch) else {
                    continue;
                };
                let mut dt =
                    OutputDamageTracker::new(capture.clipped.size, output_scale, Transform::Normal);
                if dt
                    .render_output(
                        renderer,
                        &mut target,
                        0,
                        &relocated,
                        [0.0f32, 0.0, 0.0, 1.0],
                    )
                    .is_err()
                {
                    continue;
                }
            }

            let Ok(mut target) = renderer.bind(&mut *pad_a) else {
                continue;
            };
            let Ok(mut frame) = renderer.render(&mut target, pad_live, Transform::Normal) else {
                continue;
            };
            let _ = frame.clear(Color32F::TRANSPARENT, &[Rectangle::from_size(pad_live)]);
            let src_rect: Rectangle<f64, smithay::utils::Buffer> = Rectangle::new(
                (capture.src_loc.x as f64, capture.src_loc.y as f64).into(),
                (pad_live.w as f64, pad_live.h as f64).into(),
            );
            let full = Rectangle::from_size(pad_live);
            let _ = frame.render_texture_from_to(
                &scratch,
                src_rect,
                full,
                &[full],
                &[],
                Transform::Normal,
                1.0,
                None,
                &[],
            );
            let _ = frame.finish();
        }

        // Run Kawase blur passes on the padded crop
        let offset = blur_strength * output_scale as f32;
        if render_blur(
            renderer,
            &down_shader,
            &up_shader,
            pad_a,
            pad_b,
            offset,
            blur_passes,
        )
        .is_err()
        {
            continue;
        }

        // Keep only the centre: blit the window-sized region back into
        // cache.texture, discarding the padding ring and its edge artifacts.
        {
            let blurred = pad_a.clone();
            let Ok(mut target) = renderer.bind(&mut cache.texture) else {
                continue;
            };
            let Ok(mut frame) = renderer.render(&mut target, win_size, Transform::Normal) else {
                continue;
            };
            let _ = frame.clear(Color32F::TRANSPARENT, &[Rectangle::from_size(win_size)]);
            let src_rect: Rectangle<f64, smithay::utils::Buffer> = Rectangle::new(
                (pad as f64, pad as f64).into(),
                (win_size.w as f64, win_size.h as f64).into(),
            );
            let cropped = frame
                .render_texture_from_to(
                    &blurred,
                    src_rect,
                    Rectangle::from_size(win_size),
                    &[Rectangle::from_size(win_size)],
                    &[],
                    Transform::Normal,
                    1.0,
                    None,
                    &[],
                )
                .is_ok();
            let _ = frame.finish();
            rebuilt[i] = cropped;
        }
    }

    if let Some(pool) = state.render.blur_scratch.get_mut(&output_name) {
        pool.end_frame();
    }

    // Multiply in the window's alpha shape, for every texture just rebuilt.
    for (i, req) in blur_requests.iter().enumerate() {
        if !rebuilt[i] {
            continue;
        }
        let win_size = req.screen_rect.size;
        if win_size.w <= 0 || win_size.h <= 0 {
            continue;
        }

        let prefix = match req.layer {
            BlurLayer::Overlay => overlay_prefix,
            BlurLayer::Top => top_prefix,
            BlurLayer::Pinned => pinned_prefix,
            BlurLayer::Normal => normal_prefix,
            BlurLayer::Widget => widget_prefix,
        };

        // The mask is the window's alpha shape: it changes with geometry, with
        // the live extent it was rasterized at, and with the client's blur
        // region — not with background ticks. Recapturing it per animated
        // refresh (a surface render per window per tick) made blur cost scale
        // with window count. Accepted tradeoff: an alpha-only change at
        // constant geometry and constant extent (subsurface map/unmap, a CSD
        // corner-radius change) doesn't bump `geom_gen`, so the mask stays
        // stale until something else invalidates it — rare enough not to
        // special-case.
        let key = (output_name.clone(), req.surface_id.clone());
        let regions = req.region_rects.as_deref().map(|v| v.as_slice());
        let mask_stale = mask_forced[i]
            || state.render.blur_cache.get(&key).is_none_or(|c| {
                c.mask_stamp
                    .as_ref()
                    .is_none_or(|s| !s.matches(geom_gen, win_size, regions))
            });

        let surf_start = prefix + req.elem_start;
        let surf_end = (surf_start + req.elem_count).min(all_elements.len());

        let Some(cache) = state.render.blur_cache.get_mut(&key) else {
            continue;
        };

        if mask_stale {
            // Render the surface elements straight into the window-sized mask,
            // shifted so the window origin lands at (0,0). Capturing into an
            // output-sized buffer and cropping at `screen_rect` instead left a
            // window hanging off an output edge with no captured content for
            // the off-screen strip — the crop read that buffer's mirror wrap,
            // and nothing re-captured the mask once the pan settled.
            // (index_shift is 0 here — element insertion hasn't happened yet)
            let relocated: Vec<RelocateRenderElement<&OutputRenderElements>> = all_elements
                [surf_start..surf_end]
                .iter()
                .map(|e| {
                    RelocateRenderElement::from_element(
                        e,
                        (-req.screen_rect.loc.x, -req.screen_rect.loc.y),
                        Relocate::Relative,
                    )
                })
                .collect();
            {
                let Ok(mut target) = renderer.bind(&mut cache.mask) else {
                    continue;
                };
                let mut dt = OutputDamageTracker::new(win_size, output_scale, Transform::Normal);
                // Stamping a failed capture as valid is worse than not capturing
                // at all: the multiply below bakes whatever the mask holds into
                // the frost, and the stamp then reports it fresh until the
                // window moves or resizes.
                if dt
                    .render_output(
                        renderer,
                        &mut target,
                        0,
                        &relocated,
                        [0.0f32, 0.0, 0.0, 0.0],
                    )
                    .is_err()
                {
                    continue;
                }
            }

            // `render_output` has no damage filter — it clears and draws the
            // whole target — so a client's partial blur region has to be zeroed
            // back out here. The alpha-multiply pass below then leaves those
            // pixels unfrosted.
            let outside = mask_region_complement(win_size, regions);
            if !outside.is_empty() {
                let Ok(mut target) = renderer.bind(&mut cache.mask) else {
                    continue;
                };
                let Ok(mut frame) = renderer.render(&mut target, win_size, Transform::Normal)
                else {
                    continue;
                };
                let _ = frame.clear(Color32F::TRANSPARENT, &outside);
                let _ = frame.finish();
            }
            cache.mask_stamp = Some(MaskStamp {
                geometry_generation: geom_gen,
                live: win_size,
                regions: regions.map(|r| r.to_vec()),
            });
        }

        // Masking pass — threshold surface alpha, multiply blur by it
        let Some(ref shader) = mask_shader else {
            continue;
        };
        {
            use smithay::backend::renderer::gles::ffi;
            let mask_src = cache.mask.clone();
            let Ok(mut target) = renderer.bind(&mut cache.texture) else {
                continue;
            };
            let Ok(mut frame) = renderer.render(&mut target, win_size, Transform::Normal) else {
                continue;
            };
            let _ = frame.with_context(|gl| unsafe {
                gl.Enable(ffi::BLEND);
                gl.BlendFuncSeparate(ffi::ZERO, ffi::SRC_ALPHA, ffi::ZERO, ffi::SRC_ALPHA);
            });
            let applied = frame
                .render_texture_from_to(
                    &mask_src,
                    Rectangle::from_size((win_size.w as f64, win_size.h as f64).into()),
                    Rectangle::from_size(win_size),
                    &[Rectangle::from_size(win_size)],
                    &[],
                    Transform::Normal,
                    1.0,
                    Some(shader),
                    &[],
                )
                .is_ok();
            let _ = frame.with_context(|gl| unsafe {
                gl.BlendFunc(ffi::ONE, ffi::ONE_MINUS_SRC_ALPHA);
            });
            let _ = frame.finish();
            // Without the multiply the texture is the bare blur: a full frosted
            // rectangle ignoring the window's alpha shape and its client blur
            // region. Leave it dirty so the next frame rebuilds it.
            if !applied {
                continue;
            }
        }

        // Blur texture content just changed — advance the damage snapshot so the
        // tracker re-composites the blur element on screen this frame. Only the
        // live sub-rect ever holds content; the element's `src` crops the rest
        // away anyway.
        let buf = win_size.to_logical(1).to_buffer(1, Transform::Normal);
        cache.damage_bag.add([Rectangle::from_size(buf)]);
        cache.zeroed = false;
        cache.dirty = false;
    }

    // Splice in the blur element for every window, rebuilt or cached.
    let mut index_shift = 0usize;
    for req in blur_requests.iter() {
        let win_size = req.screen_rect.size;
        if win_size.w <= 0 || win_size.h <= 0 {
            continue;
        }
        let key = (output_name.clone(), req.surface_id.clone());
        let Some(cache) = state.render.blur_cache.get(&key) else {
            continue;
        };
        if cache.zeroed {
            continue;
        }

        let prefix = match req.layer {
            BlurLayer::Overlay => overlay_prefix,
            BlurLayer::Top => top_prefix,
            BlurLayer::Pinned => pinned_prefix,
            BlurLayer::Normal => normal_prefix,
            BlurLayer::Widget => widget_prefix,
        };
        let insert_idx = prefix + req.elem_start + req.elem_count + index_shift;
        let insert_idx = insert_idx.min(all_elements.len());
        let blur_elem = TextureRenderElement::from_texture_with_damage(
            cache.id.clone(),
            context_id.clone(),
            req.screen_rect.loc.to_f64(),
            cache.texture.clone(),
            1,
            Transform::Normal,
            None,
            // The blur texture holds `win_size` *physical* texels — in its
            // top-left corner, since it is allocated to a quantized extent —
            // and is wrapped at buffer scale 1, so its src is that texel
            // sub-rect. Leaving it `None` samples only the top-left
            // 1/scale-squared and magnifies the frost on any HiDPI or zoomed
            // output; giving it the allocation instead would shrink the frost
            // and drag the dead remainder into view.
            Some(super::texel_src(win_size)),
            // Round, don't truncate: the destination has to invert back to the
            // texture's own `win_size` extent, and truncating left it up to a
            // pixel short at fractional scales, squeezing the frost at one edge.
            Some(Size::from((
                (win_size.w as f64 / output_scale).round() as i32,
                (win_size.h as f64 / output_scale).round() as i32,
            ))),
            None,
            cache.damage_bag.snapshot(),
            Kind::Unspecified,
        );
        all_elements.insert(insert_idx, OutputRenderElements::Blur(blur_elem));
        index_shift += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: i32, y: i32, w: i32, h: i32) -> Rectangle<i32, Physical> {
        Rectangle::new((x, y).into(), (w, h).into())
    }

    fn area(rects: &[Rectangle<i32, Physical>]) -> i32 {
        rects.iter().map(|r| r.size.w * r.size.h).sum()
    }

    fn any_overlap(a: &[Rectangle<i32, Physical>], b: &[Rectangle<i32, Physical>]) -> bool {
        a.iter()
            .any(|x| b.iter().any(|y| x.intersection(*y).is_some()))
    }

    #[test]
    fn no_client_region_frosts_the_whole_window() {
        assert!(mask_region_complement((200, 100).into(), None).is_empty());
    }

    #[test]
    fn a_full_window_region_leaves_nothing_to_zero() {
        let win = (200, 100).into();
        assert!(mask_region_complement(win, Some(&[rect(0, 0, 200, 100)])).is_empty());
    }

    #[test]
    fn a_partial_region_zeroes_exactly_the_rest() {
        let win: Size<i32, Physical> = (200, 100).into();
        let region = [rect(50, 20, 100, 40)];
        let outside = mask_region_complement(win, Some(&region));

        assert!(!outside.is_empty());
        assert!(!any_overlap(&outside, &region));
        assert_eq!(area(&outside), 200 * 100 - 100 * 40);
    }

    #[test]
    fn several_region_rects_are_all_kept() {
        let win: Size<i32, Physical> = (200, 100).into();
        let region = [rect(0, 0, 40, 50), rect(160, 50, 40, 50)];
        let outside = mask_region_complement(win, Some(&region));

        assert!(!any_overlap(&outside, &region));
        assert_eq!(area(&outside), 200 * 100 - 2 * 40 * 50);
    }

    #[test]
    fn region_rects_reaching_past_the_window_dont_widen_the_kept_area() {
        let win: Size<i32, Physical> = (200, 100).into();
        let outside = mask_region_complement(win, Some(&[rect(150, -20, 100, 200)]));

        assert_eq!(area(&outside), 200 * 100 - 50 * 100);
    }

    #[test]
    fn an_empty_region_list_zeroes_the_whole_mask() {
        let win: Size<i32, Physical> = (200, 100).into();
        assert_eq!(area(&mask_region_complement(win, Some(&[]))), 200 * 100);
    }

    #[test]
    fn a_padded_rect_on_the_output_is_captured_directly() {
        let capture = backdrop_capture(rect(100, 80, 300, 200), (1920, 1080).into()).unwrap();

        assert!(capture.direct);
        assert_eq!(capture.clipped, rect(100, 80, 300, 200));
        assert_eq!(capture.src_loc, Point::from((0, 0)));
    }

    #[test]
    fn a_padded_rect_flush_with_the_output_corners_is_still_direct() {
        let capture = backdrop_capture(rect(0, 0, 1920, 1080), (1920, 1080).into()).unwrap();

        assert!(capture.direct);
        assert_eq!(capture.src_loc, Point::from((0, 0)));
    }

    #[test]
    fn overhanging_the_top_left_shifts_the_crop_negative() {
        let capture = backdrop_capture(rect(-30, -20, 300, 200), (1920, 1080).into()).unwrap();

        assert!(!capture.direct);
        // The capture starts where real backdrop starts...
        assert_eq!(capture.clipped, rect(0, 0, 270, 180));
        // ...and the crop reaches that far back past it, into the mirror.
        assert_eq!(capture.src_loc, Point::from((-30, -20)));
    }

    #[test]
    fn overhanging_the_bottom_right_keeps_the_crop_at_the_origin() {
        let capture = backdrop_capture(rect(1800, 1000, 300, 200), (1920, 1080).into()).unwrap();

        assert!(!capture.direct);
        assert_eq!(capture.clipped, rect(1800, 1000, 120, 80));
        // Only the far side overhangs, so the crop starts inside the texture and
        // runs off its far edge instead.
        assert_eq!(capture.src_loc, Point::from((0, 0)));
    }

    #[test]
    fn a_padded_rect_larger_than_the_output_captures_the_whole_output() {
        let capture = backdrop_capture(rect(-100, -50, 4000, 2000), (1920, 1080).into()).unwrap();

        assert!(!capture.direct);
        assert_eq!(capture.clipped, rect(0, 0, 1920, 1080));
        assert_eq!(capture.src_loc, Point::from((-100, -50)));
    }

    #[test]
    fn a_padded_rect_off_the_output_has_nothing_to_capture() {
        let out = (1920, 1080).into();
        assert!(backdrop_capture(rect(-400, 100, 300, 200), out).is_none());
        assert!(backdrop_capture(rect(1920, 100, 300, 200), out).is_none());
        // Sharing only an edge is no overlap either — a zero-width capture has
        // no content to mirror.
        assert!(backdrop_capture(rect(-300, 100, 300, 200), out).is_none());
    }

    #[test]
    fn the_crop_always_starts_inside_the_captured_texture() {
        // The crop samples the whole padded extent from `src_loc`, so `src_loc`
        // may never run positive: that would drop real backdrop off the near
        // edge of the pad.
        let out: Size<i32, Physical> = (800, 600).into();
        for x in [-299, -1, 0, 1, 400, 799] {
            for y in [-199, -1, 0, 1, 300, 599] {
                let capture = backdrop_capture(rect(x, y, 300, 200), out).unwrap();
                assert!(capture.src_loc.x <= 0 && capture.src_loc.y <= 0);
                assert_eq!(capture.clipped.loc.x + capture.src_loc.x, x);
                assert_eq!(capture.clipped.loc.y + capture.src_loc.y, y);
            }
        }
    }

    /// A window whose own chrome is one trailing shadow element. `elem_count`
    /// stops short of it, so `behind_start` lands *on* the shadow.
    #[test]
    fn the_windows_own_shadow_is_not_behind_it() {
        assert_eq!(behind_own_chrome(7, 1, 40), 8);
    }

    /// `decoration = "none"`, fullscreen, `shadow = false`, a shadow shader that
    /// failed to compile, and every layer surface by default. Skipping anything
    /// here would step over a genuinely lower element and call an occluded
    /// window unoccluded, whose frost then silently drops that element.
    #[test]
    fn an_unshadowed_window_starts_at_the_element_right_below_it() {
        assert_eq!(behind_own_chrome(7, 0, 40), 7);
    }

    /// A layer surface can carry a border *and* a shadow below its blur.
    #[test]
    fn several_trailing_chrome_elements_are_all_skipped() {
        assert_eq!(behind_own_chrome(7, 2, 40), 9);
    }

    #[test]
    fn the_scan_never_starts_past_the_background() {
        // The bottom-most window: nothing below it but scene background, so the
        // scan range collapses to empty rather than running off the slice.
        assert_eq!(behind_own_chrome(39, 1, 40), 40);
        assert_eq!(behind_own_chrome(40, 3, 40), 40);
    }

    fn settled_backdrop() -> SharedRefreshInputs {
        SharedRefreshInputs {
            since_refresh: Some(std::time::Duration::from_millis(10)),
            view_changed: false,
            bg_structure_changed: false,
            bg_content_changed: false,
            animate_blur_fps: 20,
        }
    }

    #[test]
    fn a_backdrop_that_was_never_blurred_is_due() {
        let input = SharedRefreshInputs {
            since_refresh: None,
            ..settled_backdrop()
        };
        assert!(shared_refresh_due(&input));
    }

    #[test]
    fn a_settled_backdrop_is_not_due() {
        assert!(!shared_refresh_due(&settled_backdrop()));
    }

    #[test]
    fn a_view_change_bypasses_the_fps_cap() {
        // 10 ms into a 50 ms interval: a content change would have to wait.
        let input = SharedRefreshInputs {
            view_changed: true,
            ..settled_backdrop()
        };
        assert!(shared_refresh_due(&input));
    }

    #[test]
    fn background_content_waits_for_the_fps_cap() {
        let early = SharedRefreshInputs {
            bg_content_changed: true,
            ..settled_backdrop()
        };
        assert!(!shared_refresh_due(&early));

        let late = SharedRefreshInputs {
            since_refresh: Some(std::time::Duration::from_millis(60)),
            ..early
        };
        assert!(shared_refresh_due(&late));
    }

    #[test]
    fn zero_fps_holds_background_content_without_computing_an_interval() {
        // 1.0 / 0.0 is infinite, and `Duration::from_secs_f64` panics on it —
        // this test is the guard that the fps == 0 path returns first.
        let input = SharedRefreshInputs {
            bg_content_changed: true,
            since_refresh: Some(std::time::Duration::from_secs(3600)),
            animate_blur_fps: 0,
            ..settled_backdrop()
        };
        assert!(!shared_refresh_due(&input));
    }

    #[test]
    fn zero_fps_still_follows_the_view() {
        let input = SharedRefreshInputs {
            view_changed: true,
            animate_blur_fps: 0,
            ..settled_backdrop()
        };
        assert!(shared_refresh_due(&input));
    }

    /// The one `animate_blur_fps = 0` must not swallow: a wallpaper daemon
    /// restarting, a Background-layer surface mapping or unmapping, a wallpaper
    /// swap. Capping it strands every window on a backdrop of a background that
    /// is no longer on screen, until the camera happens to move.
    #[test]
    fn zero_fps_still_follows_a_structural_background_change() {
        let input = SharedRefreshInputs {
            bg_structure_changed: true,
            bg_content_changed: true,
            animate_blur_fps: 0,
            ..settled_backdrop()
        };
        assert!(shared_refresh_due(&input));
    }

    /// Same split at a live cap: the element set changing is not the background
    /// animating, so it does not wait for the interval either.
    #[test]
    fn a_structural_background_change_bypasses_the_fps_cap() {
        let input = SharedRefreshInputs {
            bg_structure_changed: true,
            bg_content_changed: true,
            ..settled_backdrop()
        };
        assert!(shared_refresh_due(&input));
    }

    fn signature(elements: &[(usize, usize)], ids: &[Id]) -> Vec<(Id, CommitCounter)> {
        elements
            .iter()
            .map(|&(id, commits)| (ids[id].clone(), CommitCounter::from(commits)))
            .collect()
    }

    /// A wallpaper daemon drawing every frame into one long-lived surface. This
    /// is what the fps cap is for, so it must not read as structural.
    #[test]
    fn new_content_in_the_same_elements_is_not_structural() {
        let ids: Vec<Id> = (0..3).map(|_| Id::new()).collect();
        let before = signature(&[(0, 3), (1, 7)], &ids);
        let after = signature(&[(0, 4), (1, 7)], &ids);

        assert!(!background_structure_changed(&before, &after));
        assert_ne!(before, after, "the content change still has to register");
    }

    #[test]
    fn adding_removing_or_reordering_an_element_is_structural() {
        let ids: Vec<Id> = (0..3).map(|_| Id::new()).collect();
        let before = signature(&[(0, 3), (1, 7)], &ids);

        assert!(background_structure_changed(
            &before,
            &signature(&[(0, 3), (1, 7), (2, 0)], &ids)
        ));
        assert!(background_structure_changed(
            &before,
            &signature(&[(0, 3)], &ids)
        ));
        assert!(background_structure_changed(
            &before,
            &signature(&[(1, 7), (0, 3)], &ids)
        ));
        assert!(!background_structure_changed(&before, &before.clone()));
    }

    const FINGERPRINT_PAD: i32 = 16;

    /// The frosted window every fingerprint test is taken for.
    fn frosted() -> Rectangle<i32, Physical> {
        rect(100, 100, 200, 200)
    }

    /// Squarely under it, so it is in the frost on any reading of the rect.
    fn under_the_window() -> Rectangle<i32, Physical> {
        rect(150, 150, 50, 50)
    }

    type LowerElement = (Id, CommitCounter, Rectangle<i32, Physical>);

    /// Lower elements from `(id index, commit count, geometry)`.
    fn lower(
        elements: &[(usize, usize, Rectangle<i32, Physical>)],
        ids: &[Id],
    ) -> Vec<LowerElement> {
        elements
            .iter()
            .map(|&(id, commits, geometry)| {
                (ids[id].clone(), CommitCounter::from(commits), geometry)
            })
            .collect()
    }

    /// Lower elements all sitting under the window, for the tests that are
    /// about something other than where they sit.
    fn stacked(elements: &[(usize, usize)], ids: &[Id]) -> Vec<LowerElement> {
        let placed: Vec<_> = elements
            .iter()
            .map(|&(id, commits)| (id, commits, under_the_window()))
            .collect();
        lower(&placed, ids)
    }

    fn fingerprint(
        lower: &[LowerElement],
        background: &[(Id, CommitCounter)],
    ) -> BackdropFingerprint {
        backdrop_fingerprint(
            lower
                .iter()
                .map(|(id, commit, geometry)| (id, *commit, *geometry)),
            background.iter().map(|(id, commit)| (id, *commit)),
            frosted(),
            FINGERPRINT_PAD,
            None,
        )
    }

    /// A video playing in a window below a frosted one. It redraws into one
    /// long-lived element, so nothing but the commit counter can catch it, and
    /// this is the path an overlapped window takes.
    #[test]
    fn new_content_in_a_lower_window_invalidates_the_frost() {
        let ids: Vec<Id> = (0..3).map(|_| Id::new()).collect();
        let background = signature(&[(2, 5)], &ids);

        assert_ne!(
            fingerprint(&stacked(&[(0, 3), (1, 7)], &ids), &background).hash,
            fingerprint(&stacked(&[(0, 3), (1, 8)], &ids), &background).hash,
        );
    }

    /// The same redraw in the scene background must not, or an animated
    /// wallpaper would drive every window on this path at its own frame rate —
    /// the rate `animate_blur_fps` caps on the shared backdrop's beat.
    #[test]
    fn new_content_in_the_scene_background_does_not() {
        let ids: Vec<Id> = (0..3).map(|_| Id::new()).collect();
        let lower = stacked(&[(0, 3)], &ids);

        assert_eq!(
            fingerprint(&lower, &signature(&[(1, 7), (2, 5)], &ids)).hash,
            fingerprint(&lower, &signature(&[(1, 8), (2, 5)], &ids)).hash,
        );
    }

    #[test]
    fn a_changed_element_set_registers_on_both_sides_of_the_split() {
        let ids: Vec<Id> = (0..4).map(|_| Id::new()).collect();
        let lower = stacked(&[(0, 3), (1, 7)], &ids);
        let background = signature(&[(2, 5)], &ids);
        let settled = fingerprint(&lower, &background).hash;

        for changed in [
            stacked(&[(0, 3)], &ids),
            stacked(&[(1, 7), (0, 3)], &ids),
            stacked(&[(0, 3), (1, 7), (3, 0)], &ids),
        ] {
            assert_ne!(settled, fingerprint(&changed, &background).hash);
        }
        for changed in [
            signature(&[(3, 5)], &ids),
            signature(&[(2, 5), (3, 0)], &ids),
            Vec::new(),
        ] {
            assert_ne!(settled, fingerprint(&lower, &changed).hash);
        }
    }

    /// A window redrawing *beside* a frosted one, not under it, is in none of
    /// its frost — so it must not invalidate it, and must not push it off the
    /// shared backdrop either. Otherwise a video playing next to a frosted
    /// window re-slices it every frame for a pixel-identical result.
    #[test]
    fn a_lower_element_clear_of_the_padded_capture_changes_nothing() {
        let ids: Vec<Id> = (0..2).map(|_| Id::new()).collect();
        let beside = rect(400, 100, 50, 50);

        let settled = fingerprint(&lower(&[(0, 3, beside)], &ids), &[]);
        let redrawn = fingerprint(&lower(&[(0, 4, beside)], &ids), &[]);

        assert_eq!(settled.hash, redrawn.hash);
        assert!(!settled.occluded_by_lower);
        // Its very presence is invisible too: the count is of what survived the
        // filter, so mapping or unmapping it out there is not a change either.
        assert_eq!(settled.hash, fingerprint(&[], &[]).hash);
    }

    /// The capture is padded, so the blur reach owns a band outside the window
    /// rect — an element that only reaches into that band is still in the
    /// frost, and the filter has to be the padded rect, not the window's.
    #[test]
    fn a_lower_element_reaching_into_the_padded_capture_registers() {
        let ids: Vec<Id> = (0..2).map(|_| Id::new()).collect();
        let under = under_the_window();
        let in_the_pad_band = rect(88, 88, 4, 4);

        for geometry in [under, in_the_pad_band] {
            let settled = fingerprint(&lower(&[(0, 3, geometry)], &ids), &[]);
            let redrawn = fingerprint(&lower(&[(0, 4, geometry)], &ids), &[]);

            assert_ne!(settled.hash, redrawn.hash);
            assert!(settled.occluded_by_lower);
        }
    }

    #[test]
    fn a_new_cache_takes_the_step_above_its_extent() {
        assert_eq!(quantized_extent(1, 0), 64);
        assert_eq!(quantized_extent(64, 0), 64);
        assert_eq!(quantized_extent(65, 0), 128);
        assert_eq!(quantized_extent(1920, 0), 1920);
        assert_eq!(quantized_extent(1921, 0), 1984);
    }

    #[test]
    fn growth_is_immediate() {
        assert_eq!(quantized_extent(200, 128), 256);
        // Several steps at once — a zoom-in frame can skip the ones between.
        assert_eq!(quantized_extent(1000, 128), 1024);
    }

    #[test]
    fn a_step_is_given_back_only_past_the_hysteresis_band() {
        // 192 covers up to 192; 128 covers up to 128. Anywhere in between, and
        // for half a step below 128, the allocation stays put.
        assert_eq!(quantized_extent(160, 192), 192);
        assert_eq!(quantized_extent(128, 192), 192);
        assert_eq!(quantized_extent(97, 192), 192);
        assert_eq!(quantized_extent(96, 192), 128);
    }

    #[test]
    fn the_allocation_never_drops_below_one_step() {
        assert_eq!(quantized_extent(1, 64), 64);
        assert_eq!(quantized_extent(0, 64), 64);
    }

    /// The one that matters: an extent parked on a step boundary — where a slow
    /// zoom or a resize drag leaves it — must not reallocate in both directions
    /// on alternate frames, which is the churn this quantization exists to
    /// remove.
    #[test]
    fn an_extent_hovering_on_a_boundary_reallocates_once() {
        let mut alloc = quantized_extent(128, 0);
        let mut changes = 0;
        for live in [129, 127, 130, 126, 128, 129, 127, 131, 125] {
            let next = quantized_extent(live, alloc);
            if next != alloc {
                changes += 1;
            }
            alloc = next;
        }
        assert_eq!(changes, 1);
        assert_eq!(alloc, 192);
    }

    #[test]
    fn a_continuous_zoom_reallocates_per_step_not_per_frame() {
        let mut alloc = quantized_extent(1024, 0);
        let mut changes = 0;
        // One pixel per frame is the worst case for realloc count.
        for live in (128..=1024).rev() {
            let next = quantized_extent(live, alloc);
            assert!(next <= alloc, "a shrinking extent must never grow");
            if next != alloc {
                changes += 1;
            }
            alloc = next;
        }
        assert_eq!(changes, (1024 - 192) / 64);
    }

    #[test]
    fn the_allocation_always_covers_the_live_extent() {
        for current in [0, 64, 128, 192, 1024] {
            for live in [1, 63, 64, 65, 127, 128, 129, 191, 192, 700, 1023, 1024] {
                assert!(quantized_extent(live, current) >= live);
            }
        }
    }

    fn slicers(slicers: usize, coverage: f64) -> SharedPayoff {
        SharedPayoff {
            slicers,
            coverage,
            background_empty: false,
            was_paying: false,
        }
    }

    #[test]
    fn nothing_to_slice_never_pays() {
        assert!(!shared_backdrop_pays(&SharedPayoff {
            was_paying: true,
            ..slicers(0, 4.0)
        }));
    }

    /// The flagship configuration: one frosted terminal over a static
    /// background. A full-output render plus a full-output blur per frame,
    /// where the window's own padded backdrop is a fraction of that.
    #[test]
    fn a_lone_modest_window_renders_its_own_backdrop() {
        assert!(!shared_backdrop_pays(&slicers(1, 0.42)));
    }

    /// The shared backdrop replaces one window's own render with a full-output
    /// one, so however much of the output that window covers, it never buys
    /// less work — and flipping to it would swap the frost content underneath a
    /// resize, since only the per-window capture can include what sits below
    /// the window but above the background.
    #[test]
    fn a_lone_window_over_most_of_the_output_still_renders_its_own_backdrop() {
        assert!(!shared_backdrop_pays(&slicers(1, 0.95)));
        assert!(!shared_backdrop_pays(&SharedPayoff {
            was_paying: true,
            ..slicers(1, 4.0)
        }));
    }

    /// Counting windows instead of area would take the shared path here and pay
    /// a full-output blur for two widgets covering a tenth of the screen.
    #[test]
    fn several_small_windows_still_render_their_own_backdrops() {
        assert!(!shared_backdrop_pays(&slicers(4, 0.2)));
    }

    #[test]
    fn windows_that_together_outweigh_a_full_output_pass_share_one() {
        assert!(shared_backdrop_pays(&slicers(3, 1.4)));
    }

    /// A visually-fullscreen output: the background buckets are emptied, so the
    /// shared render would blur the clear colour and any overlay-layer frost
    /// would slice a full-output rectangle of black.
    #[test]
    fn an_empty_background_never_pays() {
        assert!(!shared_backdrop_pays(&SharedPayoff {
            background_empty: true,
            was_paying: true,
            ..slicers(4, 3.0)
        }));
    }

    #[test]
    fn the_payoff_band_holds_its_decision_both_ways() {
        let between = (SHARED_CLAIM_COVERAGE + SHARED_RELEASE_COVERAGE) / 2.0;
        assert!(!shared_backdrop_pays(&slicers(2, between)));
        assert!(shared_backdrop_pays(&SharedPayoff {
            was_paying: true,
            ..slicers(2, between)
        }));
    }

    #[test]
    fn the_shared_backdrop_is_given_up_below_the_band() {
        assert!(!shared_backdrop_pays(&SharedPayoff {
            was_paying: true,
            ..slicers(2, SHARED_RELEASE_COVERAGE - 0.01)
        }));
    }

    fn captured_mask() -> MaskStamp {
        MaskStamp {
            geometry_generation: 7,
            live: (400, 300).into(),
            regions: None,
        }
    }

    #[test]
    fn an_unchanged_mask_is_reused() {
        assert!(captured_mask().matches(7, (400, 300).into(), None));
    }

    #[test]
    fn a_geometry_change_restages_the_mask() {
        assert!(!captured_mask().matches(8, (400, 300).into(), None));
    }

    /// Zoom moves the live extent without touching the geometry generation, and
    /// quantization means it can move without reallocating either. Miss this
    /// and the mask keeps the previous zoom's rasterization while the blur
    /// texture updates — an alpha multiply misaligned by the zoom step.
    #[test]
    fn a_zoom_step_inside_one_allocation_restages_the_mask() {
        assert!(!captured_mask().matches(7, (399, 300).into(), None));
    }

    /// A client can change its blur region without moving or resizing. The
    /// region lives in the mask and nowhere else, so nothing else would notice.
    #[test]
    fn a_region_change_at_rest_restages_the_mask() {
        let stamp = MaskStamp {
            regions: Some(vec![rect(0, 0, 100, 100)]),
            ..captured_mask()
        };
        let live = (400, 300).into();

        assert!(stamp.matches(7, live, Some(&[rect(0, 0, 100, 100)])));
        assert!(!stamp.matches(7, live, Some(&[rect(0, 0, 100, 200)])));
        assert!(!stamp.matches(7, live, Some(&[])));
        assert!(!stamp.matches(7, live, None));
        assert!(!captured_mask().matches(7, live, Some(&[rect(0, 0, 100, 100)])));
    }

    /// The pad pair holds exactly what the blur runs over, while the textures
    /// the *result* is kept in are quantized so a zoom stops reallocating them
    /// per frame. Slack in the pads is a strip no pass writes and
    /// `create_buffer` never initializes, sitting where the taps read.
    #[test]
    fn the_pad_pair_is_not_quantized_with_the_rest() {
        for extent in [1, 63, 65, 300, 1919] {
            let live: Size<i32, Physical> = (extent, extent).into();
            let alloc = quantized_alloc(live, Size::default());
            assert_ne!(alloc, live, "the step is what the pads must not follow");

            let pads = pad_extent(live, 16);
            assert_eq!(pads, Size::from((live.w + 32, live.h + 32)));
            assert_ne!(pads, Size::from((alloc.w + 32, alloc.h + 32)));
        }
    }

    fn at_view(x: f64, y: f64, zoom: f64) -> ViewStamp {
        ViewStamp {
            camera: Point::from((x, y)),
            zoom,
        }
    }

    /// The reason the stamp holds the f64 camera and not a counter bumped off
    /// `camera.to_i32_round()`: an easing tail moves the scene by most of a
    /// canvas unit — `zoom * output_scale` physical pixels, ~8 px at zoom 4 on a
    /// HiDPI output — between two rounded values, and the backdrop every window
    /// slices has to follow it.
    #[test]
    fn a_sub_unit_camera_drift_is_a_view_change() {
        let settled = at_view(120.0, 40.0, 4.0);
        let drifting = at_view(120.4, 40.0, 4.0);

        assert_ne!(settled, drifting);
        assert_eq!(
            settled.camera.to_i32_round::<i32>(),
            drifting.camera.to_i32_round::<i32>(),
            "the rounded camera is what missed it"
        );
    }

    #[test]
    fn a_zoom_change_alone_is_a_view_change() {
        assert_ne!(at_view(120.0, 40.0, 1.0), at_view(120.0, 40.0, 1.01));
    }

    /// The seed has to compare unequal to every real view, or the first frame
    /// would find the backdrop already up to date and slice a texture that has
    /// never been rendered.
    #[test]
    fn a_never_captured_view_matches_nothing() {
        assert_ne!(ViewStamp::never_captured(), at_view(0.0, 0.0, 1.0));
        assert_ne!(ViewStamp::never_captured(), ViewStamp::never_captured());
    }

    /// Stand-in for `acquire` without a GL context: the miss path, which is
    /// what has to make room before it takes any.
    fn acquired<T: Clone>(
        pool: &mut ScratchPool<T>,
        size: Size<i32, Physical>,
        texture: T,
        budget: usize,
    ) -> T {
        if let Some(hit) = pool.hit(size) {
            return hit;
        }
        pool.make_room(size, budget);
        pool.store(size, texture.clone());
        texture
    }

    /// The case the pool exists for: a window parked against an output edge
    /// asks for the same clipped extent every dirty frame, which during a pan
    /// is every frame.
    #[test]
    fn a_recurring_extent_is_handed_back_instead_of_reallocated() {
        let mut pool: ScratchPool<u32> = ScratchPool::default();
        let size: Size<i32, Physical> = (1920, 200).into();
        let budget = 8 * scratch_bytes(size);

        for frame in 0..10 {
            assert_eq!(acquired(&mut pool, size, frame, budget), 0);
            pool.end_frame();
        }
        assert_eq!(pool.len(), 1);
    }

    #[test]
    fn an_extent_that_stops_recurring_is_dropped() {
        let mut pool: ScratchPool<u32> = ScratchPool::default();
        let size: Size<i32, Physical> = (800, 600).into();
        acquired(&mut pool, size, 1, usize::MAX);

        for _ in 0..SCRATCH_KEEP_FRAMES {
            pool.end_frame();
        }
        assert_eq!(pool.hit(size), Some(1), "still inside the keep window");

        for _ in 0..=SCRATCH_KEEP_FRAMES {
            pool.end_frame();
        }
        assert_eq!(pool.hit(size), None);
    }

    /// One entry can be nearly output-sized, so the bound has to be in bytes: a
    /// window sliding off an edge asks for a different extent every frame.
    #[test]
    fn the_pool_stays_inside_its_byte_budget() {
        let mut pool: ScratchPool<u32> = ScratchPool::default();
        let budget = 2 * scratch_bytes((1920, 1080).into());

        for i in 0..40 {
            let size: Size<i32, Physical> = (1920, 1000 + i).into();
            // Peak, not resting: the room is made before the allocation, so
            // this bound covers the moment the new texture exists too.
            pool.make_room(size, budget);
            assert!(
                pool.held_bytes() + scratch_bytes(size) <= budget,
                "{} + {} over budget {budget}",
                pool.held_bytes(),
                scratch_bytes(size)
            );
            pool.store(size, i as u32);
        }
        assert!(pool.len() >= 1, "the newest entry always survives");
    }

    /// Eviction takes the entry that has gone longest without being asked for,
    /// not the oldest one — a bar that is still on screen must outlive the
    /// extents a moving window left behind.
    #[test]
    fn eviction_takes_the_least_recently_used_entry() {
        let mut pool: ScratchPool<u32> = ScratchPool::default();
        let bar: Size<i32, Physical> = (1920, 100).into();
        let stale: Size<i32, Physical> = (1920, 900).into();
        let budget = scratch_bytes(bar) + scratch_bytes(stale);

        acquired(&mut pool, stale, 1, budget);
        acquired(&mut pool, bar, 2, budget);
        for _ in 0..5 {
            pool.end_frame();
            pool.hit(bar);
        }

        acquired(&mut pool, (1920, 800).into(), 3, budget);
        assert_eq!(pool.hit(bar), Some(2));
        assert_eq!(pool.hit(stale), None);
    }

    #[test]
    fn a_settled_renderer_never_backs_off() {
        let mut backoff = RenderBackoff::default();
        for _ in 0..10 {
            assert!(backoff.ready());
            backoff.note_success();
        }
    }

    /// Each retry re-allocates two output-sized textures, so a renderer that is
    /// failing persistently must not be asked every frame.
    #[test]
    fn repeated_failures_back_off_further_each_time() {
        let mut backoff = RenderBackoff::default();
        let mut waits = Vec::new();

        for _ in 0..4 {
            assert!(backoff.ready());
            backoff.note_failure();
            let mut skipped = 0;
            while !backoff.ready() {
                skipped += 1;
            }
            waits.push(skipped);
        }

        assert_eq!(waits, vec![2, 4, 8, 16]);
    }

    #[test]
    fn the_backoff_is_capped_and_a_success_clears_it() {
        let mut backoff = RenderBackoff::default();
        for _ in 0..50 {
            backoff.note_failure();
        }
        let mut skipped = 0;
        while !backoff.ready() {
            skipped += 1;
        }
        assert_eq!(skipped, BACKOFF_MAX_SKIP);

        backoff.note_failure();
        backoff.note_success();
        assert!(backoff.ready(), "a working renderer is picked up at once");
    }

    #[test]
    fn blur_pad_stays_within_its_clamp() {
        // Both ends are hit in practice: the low clamp at radius 0 (blur off,
        // cache still sized) and the high clamp well before the config max, so
        // a radius edit past that point moves no cache size — which is why the
        // caches have to be dropped on reload rather than resized into.
        assert_eq!(blur_pad(1.0, 0), 16);
        assert_eq!(blur_pad(8.0, 5), 128);
        assert!((16..=128).contains(&blur_pad(4.0, 3)));
    }
}
