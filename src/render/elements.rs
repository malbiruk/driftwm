use smithay::{
    backend::renderer::{
        element::{
            Element, Id, Kind, RenderElement, UnderlyingStorage,
            memory::MemoryRenderBufferRenderElement, render_elements,
            surface::WaylandSurfaceRenderElement, texture::TextureRenderElement,
            utils::RescaleRenderElement,
        },
        gles::{
            GlesError, GlesFrame, GlesRenderer, GlesTexProgram, GlesTexture, Uniform, UniformValue,
            element::PixelShaderElement,
        },
        utils::{CommitCounter, DamageSet, OpaqueRegions},
    },
    utils::{Logical, Physical, Point, Rectangle, Scale, Size, Transform},
};

/// Render element that tiles a texture across an area using a custom GLSL shader.
/// Behaves like `PixelShaderElement` for element tracking (stable ID, area-based
/// geometry, resize/update_uniforms) but renders via `render_texture_from_to`
/// so the shader can sample the tile texture.
#[derive(Debug, Clone)]
pub struct TileShaderElement {
    shader: GlesTexProgram,
    texture: GlesTexture,
    pub tex_w: i32,
    pub tex_h: i32,
    id: Id,
    commit_counter: CommitCounter,
    area: Rectangle<i32, Logical>,
    /// Sampled sub-rect of the texture, in buffer (texel) coords; full texture
    /// unless cropped via [`set_src`](Self::set_src).
    src: Rectangle<f64, smithay::utils::Buffer>,
    opaque_regions: Vec<Rectangle<i32, Logical>>,
    alpha: f32,
    additional_uniforms: Vec<Uniform<'static>>,
    kind: Kind,
}

impl TileShaderElement {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        shader: GlesTexProgram,
        texture: GlesTexture,
        tex_w: i32,
        tex_h: i32,
        area: Rectangle<i32, Logical>,
        opaque_regions: Option<Vec<Rectangle<i32, Logical>>>,
        alpha: f32,
        additional_uniforms: Vec<Uniform<'_>>,
        kind: Kind,
    ) -> Self {
        Self {
            shader,
            texture,
            tex_w,
            tex_h,
            id: Id::new(),
            commit_counter: CommitCounter::default(),
            area,
            src: Rectangle::from_size((tex_w as f64, tex_h as f64).into()),
            opaque_regions: opaque_regions.unwrap_or_default(),
            alpha,
            additional_uniforms: additional_uniforms
                .into_iter()
                .map(|u| u.into_owned())
                .collect(),
            kind,
        }
    }

    pub fn resize(
        &mut self,
        area: Rectangle<i32, Logical>,
        opaque_regions: Option<Vec<Rectangle<i32, Logical>>>,
    ) {
        let opaque_regions = opaque_regions.unwrap_or_default();
        if self.area != area || self.opaque_regions != opaque_regions {
            self.area = area;
            self.opaque_regions = opaque_regions;
            self.commit_counter.increment();
        }
    }

    /// Crop the sampled region to a texture sub-rect (buffer/texel coords).
    /// Used to display only the interior of an apron-padded bake so edge
    /// bilinear sampling reads neighbor-continuation texels instead of clamping.
    /// No-op (no commit bump) when unchanged, so it's safe to call every frame.
    pub fn set_src(&mut self, src: Rectangle<f64, smithay::utils::Buffer>) {
        if self.src != src {
            self.src = src;
            self.commit_counter.increment();
        }
    }

    pub fn update_uniforms(&mut self, additional_uniforms: Vec<Uniform<'_>>) {
        self.additional_uniforms = additional_uniforms
            .into_iter()
            .map(|u| u.into_owned())
            .collect();
        self.commit_counter.increment();
    }
}

fn tile_corner_round(area: Rectangle<i32, Logical>, scale: Scale<f64>) -> Rectangle<i32, Physical> {
    let x0 = (area.loc.x as f64 * scale.x).round() as i32;
    let y0 = (area.loc.y as f64 * scale.y).round() as i32;
    let x1 = ((area.loc.x + area.size.w) as f64 * scale.x).round() as i32;
    let y1 = ((area.loc.y + area.size.h) as f64 * scale.y).round() as i32;
    Rectangle::new(
        Point::from((x0, y0)),
        Size::from(((x1 - x0).max(0), (y1 - y0).max(0))),
    )
}

impl Element for TileShaderElement {
    fn id(&self) -> &Id {
        &self.id
    }
    fn current_commit(&self) -> CommitCounter {
        self.commit_counter
    }

    fn src(&self) -> Rectangle<f64, smithay::utils::Buffer> {
        self.src
    }

    fn geometry(&self, scale: Scale<f64>) -> Rectangle<i32, Physical> {
        // Corner-round so adjacent chunks sharing a pre-scale edge land on
        // the same post-scale pixel — independent loc/size rounding leaves
        // 1px seams between neighbors at fractional output_scale.
        tile_corner_round(self.area, scale)
    }

    fn opaque_regions(&self, scale: Scale<f64>) -> OpaqueRegions<i32, Physical> {
        // OutputDamageTracker treats opaque regions as element-local and
        // translates them by `geometry().loc`. `self.opaque_regions` live in the
        // same absolute pre-scale space as `self.area`, so subtract the scaled
        // area origin or chunks at non-zero offsets get translated twice.
        let origin = tile_corner_round(self.area, scale).loc;
        self.opaque_regions
            .iter()
            .map(|region| {
                let mut r = tile_corner_round(*region, scale);
                r.loc -= origin;
                r
            })
            .collect()
    }

    fn alpha(&self) -> f32 {
        self.alpha
    }
    fn kind(&self) -> Kind {
        self.kind
    }
}

impl RenderElement<GlesRenderer> for TileShaderElement {
    fn draw(
        &self,
        frame: &mut GlesFrame<'_, '_>,
        src: Rectangle<f64, smithay::utils::Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        opaque_regions: &[Rectangle<i32, Physical>],
        _user_data: Option<&smithay::utils::user_data::UserDataMap>,
    ) -> Result<(), GlesError> {
        frame.render_texture_from_to(
            &self.texture,
            src,
            dst,
            damage,
            opaque_regions,
            Transform::Normal,
            self.alpha,
            Some(&self.shader),
            &self.additional_uniforms,
        )
    }

    #[inline]
    fn underlying_storage(&self, _renderer: &mut GlesRenderer) -> Option<UnderlyingStorage<'_>> {
        None
    }
}

/// Corner-rounding helper: scales a pre-zoom physical rect into a post-zoom
/// physical rect by rounding the TWO CORNERS independently (not loc+size).
///
/// Smithay's `Rectangle::to_i32_round()` rounds `loc` and `size` independently,
/// so for non-integer `scale` the resulting `right = round(loc*s) + round(size*s)`
/// can differ from `round((loc+size)*s)` by ±1 physical pixel. That off-by-one
/// is the source of black seams on window bodies at fractional zoom levels.
/// Corner rounding is pixel-consistent: adjacent elements sharing a pre-zoom
/// coordinate always meet at the same post-zoom pixel. That invariant is for
/// elements sharing an edge; [`painted_rect`] is the deliberate exception,
/// pulling chrome one pixel inside a surface's rounded-up extent at the far edges.
pub fn corner_round_rect(
    rect: Rectangle<f64, Physical>,
    scale: Scale<f64>,
) -> Rectangle<i32, Physical> {
    let x0 = (rect.loc.x * scale.x).round() as i32;
    let y0 = (rect.loc.y * scale.y).round() as i32;
    let x1 = ((rect.loc.x + rect.size.w) * scale.x).round() as i32;
    let y1 = ((rect.loc.y + rect.size.h) * scale.y).round() as i32;
    Rectangle::new(
        Point::from((x0, y0)),
        Size::from(((x1 - x0).max(0), (y1 - y0).max(0))),
    )
}

/// The physical extent a surface of this logical size actually paints: the
/// pixel-centre rule with ties rounding DOWN. See [`painted_rect`] for why.
pub fn painted_size(size: Size<f64, Logical>, scale: Scale<f64>) -> Size<i32, Physical> {
    // The fractional scale is exact (`scale_120 / 120`) but the product is not,
    // so a true tie can land a hair above .5 — `215.0 * 1.1` is
    // `236.50000000000003` — and would round up without the slack. The smallest
    // genuine non-tie fraction the fractional-scale protocol can produce is
    // 1/120, well outside this slack.
    const EPS: f64 = 1e-3;
    let extent = |size: f64, s: f64| ((size * s) - 0.5 - EPS).ceil().max(0.0) as i32;
    Size::from((extent(size.w, scale.x), extent(size.h, scale.y)))
}

/// The physical rect a surface actually paints: origin rounds the way smithay
/// places the surface, far edges follow the pixel-centre rule with ties
/// rounding DOWN (see [`painted_size`]).
///
/// `wp_fractional_scale_v1` rounds a buffer's size half away from zero, so a
/// client whose logical size lands on a half physical pixel (an odd width at
/// 1.5x) gets a buffer one texel wider than it paints, leaving that column
/// blank — chrome hugging the rounded-up extent then shows background between
/// content and border, on the right and bottom only. Integral physical extents
/// are unchanged from `to_physical_precise_round`.
pub fn painted_rect(rect: Rectangle<f64, Logical>, scale: Scale<f64>) -> Rectangle<i32, Physical> {
    Rectangle::new(
        Point::from((
            (rect.loc.x * scale.x).round() as i32,
            (rect.loc.y * scale.y).round() as i32,
        )),
        painted_size(rect.size, scale),
    )
}

/// Drop-in replacement for `smithay::backend::renderer::element::utils::RescaleRenderElement`
/// that uses pixel-snapped corner rounding (see [`corner_round_rect`]).
///
/// Used wherever a hard edge must land on the same pixel as its neighbors
/// (window surfaces, decorations, suspended chrome); shadows keep smithay's
/// default wrapper because their rasterized edges are soft.
#[derive(Debug)]
pub struct PixelSnapRescaleElement<E> {
    element: E,
    origin: Point<i32, Physical>,
    scale: Scale<f64>,
}

impl<E: Element> PixelSnapRescaleElement<E> {
    pub fn from_element(
        element: E,
        origin: Point<i32, Physical>,
        scale: impl Into<Scale<f64>>,
    ) -> Self {
        Self {
            element,
            origin,
            scale: scale.into(),
        }
    }
}

impl<E: Element> Element for PixelSnapRescaleElement<E> {
    fn id(&self) -> &smithay::backend::renderer::element::Id {
        self.element.id()
    }

    fn current_commit(&self) -> CommitCounter {
        self.element.current_commit()
    }

    fn src(&self) -> Rectangle<f64, smithay::utils::Buffer> {
        self.element.src()
    }

    fn transform(&self) -> Transform {
        self.element.transform()
    }

    fn geometry(&self, scale: Scale<f64>) -> Rectangle<i32, Physical> {
        let mut geo = self.element.geometry(scale);
        geo.loc -= self.origin;
        let mut out = corner_round_rect(geo.to_f64(), self.scale);
        out.loc += self.origin;
        out
    }

    fn damage_since(
        &self,
        scale: Scale<f64>,
        commit: Option<CommitCounter>,
    ) -> DamageSet<i32, Physical> {
        // Conservative damage: over-expand rather than under-expand so repaints
        // never miss pixels. Matches smithay's RescaleRenderElement behavior.
        self.element
            .damage_since(scale, commit)
            .into_iter()
            .map(|rect| rect.to_f64().upscale(self.scale).to_i32_up())
            .collect()
    }

    fn opaque_regions(&self, scale: Scale<f64>) -> OpaqueRegions<i32, Physical> {
        // Opaque regions must be conservative in the OTHER direction: never
        // claim a pixel is opaque unless it fully is. Shrink inward so the
        // fringe isn't mistakenly marked opaque.
        self.element
            .opaque_regions(scale)
            .into_iter()
            .map(|rect| {
                let x0 = ((rect.loc.x as f64) * self.scale.x).ceil() as i32;
                let y0 = ((rect.loc.y as f64) * self.scale.y).ceil() as i32;
                let x1 = (((rect.loc.x + rect.size.w) as f64) * self.scale.x).floor() as i32;
                let y1 = (((rect.loc.y + rect.size.h) as f64) * self.scale.y).floor() as i32;
                Rectangle::new(
                    Point::from((x0, y0)),
                    Size::from(((x1 - x0).max(0), (y1 - y0).max(0))),
                )
            })
            .collect()
    }

    fn alpha(&self) -> f32 {
        self.element.alpha()
    }

    fn kind(&self) -> Kind {
        self.element.kind()
    }

    fn is_framebuffer_effect(&self) -> bool {
        self.element.is_framebuffer_effect()
    }
}

impl<E: RenderElement<GlesRenderer>> RenderElement<GlesRenderer> for PixelSnapRescaleElement<E> {
    fn draw(
        &self,
        frame: &mut GlesFrame<'_, '_>,
        src: Rectangle<f64, smithay::utils::Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        opaque_regions: &[Rectangle<i32, Physical>],
        cache: Option<&smithay::utils::user_data::UserDataMap>,
    ) -> Result<(), GlesError> {
        self.element
            .draw(frame, src, dst, damage, opaque_regions, cache)
    }

    #[inline]
    fn underlying_storage(&self, renderer: &mut GlesRenderer) -> Option<UnderlyingStorage<'_>> {
        self.element.underlying_storage(renderer)
    }

    fn capture_framebuffer(
        &self,
        frame: &mut GlesFrame<'_, '_>,
        src: Rectangle<f64, smithay::utils::Buffer>,
        dst: Rectangle<i32, Physical>,
        cache: &smithay::utils::user_data::UserDataMap,
    ) -> Result<(), GlesError> {
        self.element.capture_framebuffer(frame, src, dst, cache)
    }
}

/// Stretches an element into `dst` instead of the rect its own rounding would
/// produce — normally [`painted_rect`] of the same logical rect the window's
/// border uses, so a title bar or body fill ends where the content does rather
/// than on top of the border's inner stroke.
///
/// Stretches rather than crops so the buffer's own rounded corners stay
/// centred on the border's arc; decoration buffers are supersampled, so the
/// resampling isn't visible.
#[derive(Debug)]
pub struct TrimmedElement<E> {
    element: E,
    dst: Rectangle<i32, Physical>,
}

impl<E: Element> TrimmedElement<E> {
    pub fn from_element(element: E, dst: Rectangle<i32, Physical>) -> Self {
        Self { element, dst }
    }
}

impl<E: Element> Element for TrimmedElement<E> {
    fn id(&self) -> &Id {
        self.element.id()
    }

    fn current_commit(&self) -> CommitCounter {
        self.element.current_commit()
    }

    fn src(&self) -> Rectangle<f64, smithay::utils::Buffer> {
        self.element.src()
    }

    fn transform(&self) -> Transform {
        self.element.transform()
    }

    fn geometry(&self, _scale: Scale<f64>) -> Rectangle<i32, Physical> {
        self.dst
    }

    fn damage_since(
        &self,
        scale: Scale<f64>,
        commit: Option<CommitCounter>,
    ) -> DamageSet<i32, Physical> {
        // Rebased onto `dst`, not clipped to it: over-damaging by the trimmed
        // sliver only costs a repaint.
        let offset = self.element.geometry(scale).loc - self.dst.loc;
        self.element
            .damage_since(scale, commit)
            .into_iter()
            .map(|mut rect| {
                rect.loc += offset;
                rect
            })
            .collect()
    }

    fn opaque_regions(&self, scale: Scale<f64>) -> OpaqueRegions<i32, Physical> {
        // The damage tracker translates opaque regions by `geometry().loc` but
        // never clips them to it, so an untrimmed region would keep claiming
        // the pixels this element no longer covers.
        let inner = self.element.geometry(scale);
        self.element
            .opaque_regions(scale)
            .into_iter()
            .filter_map(|mut rect| {
                rect.loc += inner.loc;
                let mut clipped = rect.intersection(self.dst)?;
                clipped.loc -= self.dst.loc;
                Some(clipped)
            })
            .collect()
    }

    fn alpha(&self) -> f32 {
        self.element.alpha()
    }

    fn kind(&self) -> Kind {
        self.element.kind()
    }

    fn is_framebuffer_effect(&self) -> bool {
        self.element.is_framebuffer_effect()
    }
}

impl<E: RenderElement<GlesRenderer>> RenderElement<GlesRenderer> for TrimmedElement<E> {
    fn draw(
        &self,
        frame: &mut GlesFrame<'_, '_>,
        src: Rectangle<f64, smithay::utils::Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        opaque_regions: &[Rectangle<i32, Physical>],
        cache: Option<&smithay::utils::user_data::UserDataMap>,
    ) -> Result<(), GlesError> {
        self.element
            .draw(frame, src, dst, damage, opaque_regions, cache)
    }

    #[inline]
    fn underlying_storage(&self, renderer: &mut GlesRenderer) -> Option<UnderlyingStorage<'_>> {
        self.element.underlying_storage(renderer)
    }

    fn capture_framebuffer(
        &self,
        frame: &mut GlesFrame<'_, '_>,
        src: Rectangle<f64, smithay::utils::Buffer>,
        dst: Rectangle<i32, Physical>,
        cache: &smithay::utils::user_data::UserDataMap,
    ) -> Result<(), GlesError> {
        self.element.capture_framebuffer(frame, src, dst, cache)
    }
}

/// Per-window affine transform applied on top of the camera zoom, so the
/// canvas transform and the window-local lifecycle/geometry animation stay
/// independent. `origin` and `offset` are physical; `scale` is the visual
/// stretch. Wraps the already-zoomed element.
#[derive(Debug)]
pub struct WindowTransformElement<E> {
    element: E,
    origin: Point<f64, Physical>,
    offset: Point<f64, Physical>,
    scale: Scale<f64>,
}

impl<E> WindowTransformElement<E> {
    pub fn new(
        element: E,
        origin: Point<f64, Physical>,
        offset: Point<f64, Physical>,
        scale: Scale<f64>,
    ) -> Self {
        Self {
            element,
            origin,
            offset,
            scale,
        }
    }

    fn transform_rect(&self, rect: Rectangle<i32, Physical>) -> Rectangle<i32, Physical> {
        let x0 = self.origin.x + (rect.loc.x as f64 - self.origin.x) * self.scale.x + self.offset.x;
        let y0 = self.origin.y + (rect.loc.y as f64 - self.origin.y) * self.scale.y + self.offset.y;
        let x1 = self.origin.x
            + ((rect.loc.x + rect.size.w) as f64 - self.origin.x) * self.scale.x
            + self.offset.x;
        let y1 = self.origin.y
            + ((rect.loc.y + rect.size.h) as f64 - self.origin.y) * self.scale.y
            + self.offset.y;
        Rectangle::new(
            Point::from((x0.round() as i32, y0.round() as i32)),
            Size::from((
                (x1.round() as i32 - x0.round() as i32).max(0),
                (y1.round() as i32 - y0.round() as i32).max(0),
            )),
        )
    }
}

impl<E: Element> Element for WindowTransformElement<E> {
    fn id(&self) -> &Id {
        self.element.id()
    }
    fn current_commit(&self) -> CommitCounter {
        self.element.current_commit()
    }
    fn src(&self) -> Rectangle<f64, smithay::utils::Buffer> {
        self.element.src()
    }
    fn transform(&self) -> Transform {
        self.element.transform()
    }
    fn geometry(&self, scale: Scale<f64>) -> Rectangle<i32, Physical> {
        self.transform_rect(self.element.geometry(scale))
    }
    fn damage_since(
        &self,
        scale: Scale<f64>,
        commit: Option<CommitCounter>,
    ) -> DamageSet<i32, Physical> {
        // Element-relative damage upscaled by the local stretch; the damage
        // tracker's geometry/alpha diffing supplies the per-frame motion damage.
        self.element
            .damage_since(scale, commit)
            .into_iter()
            .map(|rect| rect.to_f64().upscale(self.scale).to_i32_up())
            .collect()
    }
    fn opaque_regions(&self, _scale: Scale<f64>) -> OpaqueRegions<i32, Physical> {
        // Animated alpha and fractional transforms make conservative opaque
        // tracking safer than a small occlusion optimization.
        OpaqueRegions::default()
    }
    fn alpha(&self) -> f32 {
        self.element.alpha()
    }
    fn kind(&self) -> Kind {
        self.element.kind()
    }
}

impl<E: RenderElement<GlesRenderer>> RenderElement<GlesRenderer> for WindowTransformElement<E> {
    fn draw(
        &self,
        frame: &mut GlesFrame<'_, '_>,
        src: Rectangle<f64, smithay::utils::Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        opaque_regions: &[Rectangle<i32, Physical>],
        cache: Option<&smithay::utils::user_data::UserDataMap>,
    ) -> Result<(), GlesError> {
        self.element
            .draw(frame, src, dst, damage, opaque_regions, cache)
    }

    fn underlying_storage(&self, renderer: &mut GlesRenderer) -> Option<UnderlyingStorage<'_>> {
        self.element.underlying_storage(renderer)
    }
}

render_elements! {
    pub OutputRenderElements<=GlesRenderer>;
    Background=RescaleRenderElement<PixelShaderElement>,
    TileBg=RescaleRenderElement<TileShaderElement>,
    // PixelSnap (not Rescale): chunks need a shared rounding anchor to meet
    // at pixel-consistent edges at fractional zoom.
    TileBgChunk=PixelSnapRescaleElement<TileShaderElement>,
    WallpaperBg=TileShaderElement,
    Decoration=PixelSnapRescaleElement<MemoryRenderBufferRenderElement<GlesRenderer>>,
    // Chrome bordering window content (title bar, stand-in body) is trimmed to
    // the painted rect; the error bar and viewport outlines border nothing and
    // stay untrimmed.
    TrimmedDecoration=PixelSnapRescaleElement<TrimmedElement<MemoryRenderBufferRenderElement<GlesRenderer>>>,
    Window=PixelSnapRescaleElement<WaylandSurfaceRenderElement<GlesRenderer>>,
    CsdWindow=PixelSnapRescaleElement<RoundedCornerElement>,
    AnimatedDecoration=WindowTransformElement<PixelSnapRescaleElement<MemoryRenderBufferRenderElement<GlesRenderer>>>,
    AnimatedTrimmedDecoration=WindowTransformElement<PixelSnapRescaleElement<TrimmedElement<MemoryRenderBufferRenderElement<GlesRenderer>>>>,
    AnimatedWindow=WindowTransformElement<PixelSnapRescaleElement<WaylandSurfaceRenderElement<GlesRenderer>>>,
    AnimatedCsdWindow=WindowTransformElement<PixelSnapRescaleElement<RoundedCornerElement>>,
    AnimatedChrome=WindowTransformElement<RescaleRenderElement<PixelShaderElement>>,
    ClosingWindow=WindowTransformElement<TextureRenderElement<GlesTexture>>,
    Layer=WaylandSurfaceRenderElement<GlesRenderer>,
    Cursor=MemoryRenderBufferRenderElement<GlesRenderer>,
    CursorSurface=smithay::backend::renderer::element::Wrap<WaylandSurfaceRenderElement<GlesRenderer>>,
    Blur=TextureRenderElement<GlesTexture>,
}

// Shadow and Decoration share inner types with Background and Tile respectively.
// We can't add them to render_elements! because it generates conflicting From impls.
// Instead we construct them directly using the existing Background/Tile variants.
// Helpers below create the elements and wrap them in the correct variant.

/// Wraps a `WaylandSurfaceRenderElement` and clips it to a rounded-rectangle
/// geometry shared by all elements of the same window. Every surface of a
/// window (toplevel + subsurfaces) is wrapped, so the clip applies uniformly
/// even when the client renders content into a subsurface (Firefox, apps
/// with HW-accelerated video/GL).
///
/// Storage:
/// - `geometry` is in screen-logical pre-zoom coords (output-relative),
///   same coord space as the element location passed at build time.
/// - `corner_radius` is `(top_left, top_right, bottom_right, bottom_left)`
///   in logical pixels (matches the `geo_size` uniform units; the shader
///   multiplies both against `aa_scale` for the AA band).
/// - `output_scale` feeds `inner.geometry(scale)` at draw time to get the
///   element's pre-zoom physical rect in the same space as [`painted_rect`]
///   of `geometry`, which is what the clip boundary follows.
/// - `aa_scale` is `output_scale * zoom` — keeps the AA band ~1 output
///   pixel wide regardless of canvas zoom.
pub struct RoundedCornerElement {
    inner: WaylandSurfaceRenderElement<GlesRenderer>,
    shader: GlesTexProgram,
    geometry: Rectangle<f64, Logical>,
    corner_radius: [f32; 4],
    output_scale: f64,
    aa_scale: f32,
}

impl RoundedCornerElement {
    pub fn new(
        inner: WaylandSurfaceRenderElement<GlesRenderer>,
        shader: GlesTexProgram,
        geometry: Rectangle<f64, Logical>,
        corner_radius: [f32; 4],
        output_scale: f64,
        aa_scale: f32,
    ) -> Self {
        Self {
            inner,
            shader,
            geometry,
            corner_radius,
            output_scale,
            aa_scale,
        }
    }

    fn has_rounding(&self) -> bool {
        self.corner_radius.iter().any(|r| *r > 0.0)
    }

    /// Per-corner square cut-out rects in geometry-local physical pixels at
    /// the given scale. Used for `opaque_regions`; +1 pixel covers the
    /// smoothstep fringe so we never claim a fading pixel as opaque.
    /// Zero-radius corners produce zero-sized rects (no cut).
    fn corner_cutouts(&self, scale: Scale<f64>) -> [Rectangle<i32, Physical>; 4] {
        let geo = painted_rect(self.geometry, scale);
        let r_px = |r: f32| {
            if r <= 0.0 {
                0
            } else {
                (r as f64 * scale.x).ceil() as i32 + 1
            }
        };
        let (w, h) = (geo.size.w, geo.size.h);
        let rtl = r_px(self.corner_radius[0]);
        let rtr = r_px(self.corner_radius[1]);
        let rbr = r_px(self.corner_radius[2]);
        let rbl = r_px(self.corner_radius[3]);
        [
            Rectangle::new((0, 0).into(), (rtl, rtl).into()),
            Rectangle::new((w - rtr, 0).into(), (rtr, rtr).into()),
            Rectangle::new((w - rbr, h - rbr).into(), (rbr, rbr).into()),
            Rectangle::new((0, h - rbl).into(), (rbl, rbl).into()),
        ]
    }

    fn compute_uniforms(&self) -> Vec<Uniform<'static>> {
        // Matrix uses physical units throughout — the ratios cancel when
        // normalizing to geo-space, so units don't matter as long as both
        // elem_geo and geo are the same. geo_size/corner_radius uniforms
        // must be in logical pixels to pair with `aa_scale = output_scale
        // * zoom`, so the shader's AA band lands at one output pixel.
        let scale = Scale::from(self.output_scale);
        let elem_geo = self.inner.geometry(scale);
        let geo = painted_rect(self.geometry, scale);

        let elem_x = elem_geo.loc.x as f32;
        let elem_y = elem_geo.loc.y as f32;
        let elem_w = elem_geo.size.w.max(1) as f32;
        let elem_h = elem_geo.size.h.max(1) as f32;

        let geo_x = geo.loc.x as f32;
        let geo_y = geo.loc.y as f32;
        let geo_w = geo.size.w.max(1) as f32;
        let geo_h = geo.size.h.max(1) as f32;

        let buf = self.inner.buffer_size();
        let buf_w = (buf.w.max(1)) as f32;
        let buf_h = (buf.h.max(1)) as f32;

        let view = self.inner.view();
        let src_x = view.src.loc.x as f32;
        let src_y = view.src.loc.y as f32;
        let src_w = (view.src.size.w.max(1.0)) as f32;
        let src_h = (view.src.size.h.max(1.0)) as f32;

        // Combined matrix: buffer_uv → geometry-normalized [0,1]².
        //   uv → (uv * buf - src_loc) / src           (undo viewporter)
        //      → src_uv * elem / geo + (elem_loc - geo_loc) / geo
        let sx = (buf_w / src_w) * (elem_w / geo_w);
        let sy = (buf_h / src_h) * (elem_h / geo_h);
        let tx = -(src_x / src_w) * (elem_w / geo_w) + (elem_x - geo_x) / geo_w;
        let ty = -(src_y / src_h) * (elem_h / geo_h) + (elem_y - geo_y) / geo_h;

        // Column-major 3x3: cols stored back-to-back.
        let input_to_geo: [f32; 9] = [sx, 0.0, 0.0, 0.0, sy, 0.0, tx, ty, 1.0];

        // Logical size of the painted rect, not of `self.geometry`, so the clip
        // arc and the border's arc share a centre.
        let geo_size_logical = (
            geo.size.w as f32 / self.output_scale as f32,
            geo.size.h as f32 / self.output_scale as f32,
        );

        vec![
            Uniform::new("aa_scale", self.aa_scale),
            Uniform::new("geo_size", geo_size_logical),
            Uniform::new(
                "corner_radius",
                (
                    self.corner_radius[0],
                    self.corner_radius[1],
                    self.corner_radius[2],
                    self.corner_radius[3],
                ),
            ),
            Uniform::new(
                "input_to_geo",
                UniformValue::Matrix3x3 {
                    matrices: vec![input_to_geo],
                    transpose: false,
                },
            ),
        ]
    }
}

impl Element for RoundedCornerElement {
    fn id(&self) -> &Id {
        self.inner.id()
    }
    fn current_commit(&self) -> CommitCounter {
        self.inner.current_commit()
    }
    fn location(&self, scale: Scale<f64>) -> Point<i32, Physical> {
        self.inner.location(scale)
    }
    fn src(&self) -> Rectangle<f64, smithay::utils::Buffer> {
        self.inner.src()
    }
    fn transform(&self) -> Transform {
        self.inner.transform()
    }
    fn geometry(&self, scale: Scale<f64>) -> Rectangle<i32, Physical> {
        self.inner.geometry(scale)
    }

    fn damage_since(
        &self,
        scale: Scale<f64>,
        commit: Option<CommitCounter>,
    ) -> DamageSet<i32, Physical> {
        // Damage intersected with the clipped region: pixels outside geometry
        // are zeroed by the shader, so damage there can never change output.
        let damage = self.inner.damage_since(scale, commit);
        let mut geo = painted_rect(self.geometry, scale);
        geo.loc -= self.geometry(scale).loc;
        damage
            .into_iter()
            .filter_map(|rect| rect.intersection(geo))
            .collect()
    }

    fn opaque_regions(&self, scale: Scale<f64>) -> OpaqueRegions<i32, Physical> {
        let regions = self.inner.opaque_regions(scale);
        if regions.is_empty() {
            return regions;
        }
        // Translate geometry rect to be relative to the element's origin
        // (opaque_regions are element-local in smithay's convention).
        let mut geo = painted_rect(self.geometry, scale);
        geo.loc -= self.geometry(scale).loc;
        // GTK4 mis-reports the full surface as opaque even though the rim
        // column has partial alpha from anti-aliased GSK rasterization;
        // smithay's no-blend path then writes those PMA values to the
        // framebuffer raw, producing a 1-px dark line at the right/bottom
        // edge. Shrink the opaque region by 1 physical pixel on every side
        // so the rim always alpha-blends.
        if geo.size.w > 2 && geo.size.h > 2 {
            geo.loc.x += 1;
            geo.loc.y += 1;
            geo.size.w -= 2;
            geo.size.h -= 2;
        } else {
            return OpaqueRegions::default();
        }
        let clipped: Vec<_> = regions
            .into_iter()
            .filter_map(|rect| rect.intersection(geo))
            .collect();
        if clipped.is_empty() || !self.has_rounding() {
            return clipped.into_iter().collect();
        }
        // Subtract the rounded-corner square cutouts (in geometry-local
        // coords) offset into element-local coords.
        let offset = geo.loc;
        let corners: Vec<Rectangle<i32, Physical>> = self
            .corner_cutouts(scale)
            .into_iter()
            .map(|mut r| {
                r.loc += offset;
                r
            })
            .collect();
        Rectangle::subtract_rects_many(clipped, corners)
            .into_iter()
            .collect()
    }

    fn alpha(&self) -> f32 {
        self.inner.alpha()
    }
    fn kind(&self) -> Kind {
        self.inner.kind()
    }
}

impl RenderElement<GlesRenderer> for RoundedCornerElement {
    fn draw(
        &self,
        frame: &mut GlesFrame<'_, '_>,
        src: Rectangle<f64, smithay::utils::Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        opaque_regions: &[Rectangle<i32, Physical>],
        user_data: Option<&smithay::utils::user_data::UserDataMap>,
    ) -> Result<(), GlesError> {
        // The input_to_geo math doesn't compensate for non-identity buffer
        // transforms. For rotated/flipped surfaces we'd clip against the
        // wrong edges — fall back to the default tex program so at least
        // the content is visible. No driftwm-supported client sets this
        // today; if one starts to, extend `compute_uniforms` with a
        // transform-aware UV→geo matrix.
        if self.inner.transform() != Transform::Normal {
            return self
                .inner
                .draw(frame, src, dst, damage, opaque_regions, user_data);
        }
        frame.override_default_tex_program(self.shader.clone(), self.compute_uniforms());
        let result = self
            .inner
            .draw(frame, src, dst, damage, opaque_regions, user_data);
        frame.clear_tex_program_override();
        result
    }

    fn underlying_storage(&self, renderer: &mut GlesRenderer) -> Option<UnderlyingStorage<'_>> {
        self.inner.underlying_storage(renderer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn logical(x: f64, y: f64, w: f64, h: f64) -> Rectangle<f64, Logical> {
        Rectangle::new(Point::from((x, y)), Size::from((w, h)))
    }

    #[test]
    fn painted_rect_matches_rounding_off_a_tie() {
        for (rect, scale) in [
            (logical(10.0, 20.0, 100.0, 50.0), 2.0),
            (logical(10.0, 20.0, 1692.0, 1052.0), 1.5),
            (logical(0.0, 0.0, 203.0, 203.0), 1.25),
        ] {
            let scale = Scale::from(scale);
            let rounded: Rectangle<i32, Physical> = rect.to_physical_precise_round(scale);
            assert_eq!(painted_rect(rect, scale), rounded, "{rect:?} at {scale:?}");
        }
    }

    #[test]
    fn painted_rect_rounds_a_tie_down_and_the_origin_normally() {
        let rect = logical(10.6, 10.4, 1693.0, 1053.0);
        let scale = Scale::from(1.5);
        let rounded: Rectangle<i32, Physical> = rect.to_physical_precise_round(scale);

        let painted = painted_rect(rect, scale);
        assert_eq!(painted.loc, rounded.loc);
        assert_eq!(painted.size, Size::from((2539, 1579)));
        assert_eq!(rounded.size, Size::from((2540, 1580)));

        assert_eq!(
            painted_rect(rect, Scale::from(1.0)).loc,
            Point::from((11, 10))
        );
    }

    #[test]
    fn painted_rect_epsilon_survives_float_noise() {
        // The mathematical tie 215 * 1.1 = 236.5 is not what f64 computes, so
        // the naive form rounds it up.
        assert_eq!((215.0f64 * 1.1 - 0.5).ceil() as i32, 237);
        assert_eq!(
            painted_rect(logical(0.0, 0.0, 215.0, 215.0), Scale::from(1.1))
                .size
                .w,
            236
        );
    }

    #[test]
    fn painted_rect_of_an_ssd_body_keeps_the_bar_band_whole() {
        let scale = Scale::from(1.5);
        let bar = 60.0 / 1.5;
        let painted = painted_rect(logical(10.6, 10.4 - bar, 1693.0, 1053.0 + bar), scale);
        let content_top = (10.4f64 * 1.5).round() as i32;
        assert_eq!(painted.loc.y, content_top - 60);
        assert_eq!(painted.loc.y + painted.size.h, content_top + 1579);
    }

    #[test]
    fn painted_rect_never_gaps_behind_the_content_at_zoom() {
        let rect = logical(0.0, 0.0, 1693.0, 1053.0);
        let scale = Scale::from(1.5);
        let surface: Rectangle<i32, Physical> = rect.to_physical_precise_round(scale);
        let painted = painted_rect(rect, scale);

        for zoom in [0.5, 0.8, 1.0, 1.3, 3.0] {
            let zoom_scale = Scale::from(zoom);
            let content = corner_round_rect(surface.to_f64(), zoom_scale);
            let ring = corner_round_rect(painted.to_f64(), zoom_scale);
            // The ring's inner edge sits at or inside the content's, so no
            // background can show between them. The overlap is bounded by the
            // one pre-zoom column the clip shader discards.
            for (ring_far, content_far) in [
                (ring.loc.x + ring.size.w, content.loc.x + content.size.w),
                (ring.loc.y + ring.size.h, content.loc.y + content.size.h),
            ] {
                let pulled_in = content_far - ring_far;
                assert!(
                    (0..=zoom.ceil() as i32).contains(&pulled_in),
                    "zoom {zoom}: pulled in by {pulled_in}"
                );
                // Below 1 the two far edges can legitimately land on the same
                // pixel; from 1 up the tie column must actually be given back.
                if zoom >= 1.0 {
                    assert!(pulled_in >= 1, "zoom {zoom}: nothing pulled in");
                }
            }
        }
    }

    #[derive(Debug)]
    struct StubElement {
        id: Id,
        geometry: Rectangle<i32, Physical>,
        opaque: Vec<Rectangle<i32, Physical>>,
    }

    impl Element for StubElement {
        fn id(&self) -> &Id {
            &self.id
        }
        fn current_commit(&self) -> CommitCounter {
            CommitCounter::default()
        }
        fn src(&self) -> Rectangle<f64, smithay::utils::Buffer> {
            Rectangle::from_size(Size::from((1.0, 1.0)))
        }
        fn geometry(&self, _scale: Scale<f64>) -> Rectangle<i32, Physical> {
            self.geometry
        }
        fn opaque_regions(&self, _scale: Scale<f64>) -> OpaqueRegions<i32, Physical> {
            OpaqueRegions::from_slice(&self.opaque)
        }
    }

    /// The close-fade bake trims its SSD bar to the painted width but keeps the
    /// element's own height, because the bar's bottom is the content's top edge
    /// rather than a ring edge. A tie-height bar (25 logical at 1.5x) would
    /// otherwise leave a transparent row across the window for the whole fade.
    #[test]
    fn a_baked_bar_meets_the_content_it_sits_on() {
        let scale = Scale::from(1.5);
        let loc = Point::<f64, Logical>::from((0.0, 12.0));
        let size = Size::<f64, Logical>::from((101.0, 25.0));

        // What smithay's memory element reports: `round(size*s + loc) - round(loc)`.
        let top = (loc.y * scale.y).round() as i32;
        let element_height = ((size.h * scale.y + loc.y * scale.y).round() as i32) - top;
        let inner = Rectangle::new(
            Point::<i32, Physical>::from(((loc.x * scale.x).round() as i32, top)),
            Size::from((
                ((size.w * scale.x + loc.x * scale.x).round() as i32)
                    - (loc.x * scale.x).round() as i32,
                element_height,
            )),
        );

        let mut dst = painted_rect(Rectangle::new(loc, size), scale);
        dst.size.h = inner.size.h;

        let bar = TrimmedElement::from_element(
            StubElement {
                id: Id::new(),
                geometry: inner,
                opaque: Vec::new(),
            },
            dst,
        );
        let bar_bottom = bar.geometry(scale).loc.y + bar.geometry(scale).size.h;
        assert_eq!(bar_bottom, ((loc.y + size.h) * scale.y).round() as i32);
        // The far x edge is a ring edge, so it does pull in.
        assert_eq!(bar.geometry(scale).size.w, inner.size.w - 1);
    }

    #[test]
    fn trimmed_element_reports_and_clips_to_its_dst() {
        let scale = Scale::from(1.5);
        let inner = Rectangle::new(
            Point::<i32, Physical>::from((16, 16)),
            Size::from((2540, 1580)),
        );
        let dst = Rectangle::new(inner.loc, Size::from((2539, 1579)));
        let element = TrimmedElement::from_element(
            StubElement {
                id: Id::new(),
                geometry: inner,
                opaque: vec![Rectangle::from_size(inner.size)],
            },
            dst,
        );

        assert_eq!(element.geometry(scale), dst);

        let regions: Vec<_> = element.opaque_regions(scale).into_iter().collect();
        assert_eq!(regions, vec![Rectangle::from_size(dst.size)]);
    }
}
