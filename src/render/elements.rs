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

    pub fn update_uniforms(&mut self, additional_uniforms: Vec<Uniform<'_>>) {
        self.additional_uniforms = additional_uniforms
            .into_iter()
            .map(|u| u.into_owned())
            .collect();
        self.commit_counter.increment();
    }
}

impl Element for TileShaderElement {
    fn id(&self) -> &Id {
        &self.id
    }
    fn current_commit(&self) -> CommitCounter {
        self.commit_counter
    }

    fn src(&self) -> Rectangle<f64, smithay::utils::Buffer> {
        Rectangle::from_size((self.tex_w as f64, self.tex_h as f64).into())
    }

    fn geometry(&self, scale: Scale<f64>) -> Rectangle<i32, Physical> {
        self.area.to_physical_precise_round(scale)
    }

    fn opaque_regions(&self, scale: Scale<f64>) -> OpaqueRegions<i32, Physical> {
        self.opaque_regions
            .iter()
            .map(|region| region.to_physical_precise_round(scale))
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
/// coordinate always meet at the same post-zoom pixel.
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

/// Drop-in replacement for `smithay::backend::renderer::element::utils::RescaleRenderElement`
/// that uses pixel-snapped corner rounding (see [`corner_round_rect`]).
///
/// Used only for window surface elements — shadows and decorations keep smithay's
/// default wrapper because their rasterized edges are either soft (shadow) or
/// small bitmaps where the ±1 pixel isn't visible (title bars).
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

render_elements! {
    pub OutputRenderElements<=GlesRenderer>;
    Background=RescaleRenderElement<PixelShaderElement>,
    TileBg=RescaleRenderElement<TileShaderElement>,
    WallpaperBg=TileShaderElement,
    Decoration=PixelSnapRescaleElement<MemoryRenderBufferRenderElement<GlesRenderer>>,
    Window=PixelSnapRescaleElement<WaylandSurfaceRenderElement<GlesRenderer>>,
    CsdWindow=PixelSnapRescaleElement<RoundedCornerElement>,
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
///   element's pre-zoom physical rect in the same space as `geometry`
///   converted via `to_physical_precise_round(output_scale)`.
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
        let geo: Rectangle<i32, Physical> = self.geometry.to_physical_precise_round(scale);
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
        let geo: Rectangle<i32, Physical> = self.geometry.to_physical_precise_round(scale);

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

        let geo_size_logical = (self.geometry.size.w as f32, self.geometry.size.h as f32);

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
        let mut geo = self.geometry.to_physical_precise_round(scale);
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
        let mut geo = self.geometry.to_physical_precise_round(scale);
        geo.loc -= self.geometry(scale).loc;
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
