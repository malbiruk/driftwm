use std::borrow::Cow;
use std::time::Duration;

use smithay::{
    backend::renderer::{
        element::{
            Element, RenderElement,
            Kind,
            memory::MemoryRenderBufferRenderElement,
            render_elements,
            texture::TextureRenderElement,
            utils::RescaleRenderElement,
        },
        gles::{GlesError, GlesFrame, GlesRenderer, GlesTexProgram, GlesTexture, Uniform, UniformName, UniformType, element::PixelShaderElement},
        utils::{CommitCounter, DamageSet, OpaqueRegions},
    },
    input::pointer::{CursorImageStatus, CursorImageSurfaceData},
    output::Output,
    utils::{Logical, Physical, Point, Rectangle, Scale},
};

use smithay::backend::renderer::element::AsRenderElements;
use smithay::backend::renderer::element::surface::WaylandSurfaceRenderElement;
use smithay::desktop::layer_map_for_output;
use smithay::wayland::shell::wlr_layer::Layer as WlrLayer;

use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::element::memory::MemoryRenderBuffer;
use smithay::utils::{Size, Transform};

use smithay::reexports::wayland_server::Resource;
use smithay::utils::IsAlive;
use smithay::wayland::compositor::with_states;
use smithay::wayland::seat::WaylandFocus;

use smithay::backend::renderer::element::{Id, UnderlyingStorage};

use driftwm::canvas::{self, CanvasPos, canvas_to_screen};

/// Render element that tiles a texture across an area using a custom GLSL shader.
/// Behaves like `PixelShaderElement` for element tracking (stable ID, area-based
/// geometry, resize/update_uniforms) but renders via `render_texture_from_to`
/// so the shader can sample the tile texture.
#[derive(Debug, Clone)]
pub struct TileShaderElement {
    shader: GlesTexProgram,
    texture: GlesTexture,
    tex_w: i32,
    tex_h: i32,
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
            additional_uniforms: additional_uniforms.into_iter().map(|u| u.into_owned()).collect(),
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
        self.additional_uniforms = additional_uniforms.into_iter().map(|u| u.into_owned()).collect();
        self.commit_counter.increment();
    }
}

impl Element for TileShaderElement {
    fn id(&self) -> &Id { &self.id }
    fn current_commit(&self) -> CommitCounter { self.commit_counter }

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

    fn alpha(&self) -> f32 { self.alpha }
    fn kind(&self) -> Kind { self.kind }
}

impl RenderElement<GlesRenderer> for TileShaderElement {
    fn draw(
        &self,
        frame: &mut GlesFrame<'_, '_>,
        src: Rectangle<f64, smithay::utils::Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        opaque_regions: &[Rectangle<i32, Physical>],
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

render_elements! {
    pub OutputRenderElements<=GlesRenderer>;
    Background=RescaleRenderElement<PixelShaderElement>,
    TileBg=RescaleRenderElement<TileShaderElement>,
    Decoration=RescaleRenderElement<MemoryRenderBufferRenderElement<GlesRenderer>>,
    Window=RescaleRenderElement<WaylandSurfaceRenderElement<GlesRenderer>>,
    CsdWindow=RescaleRenderElement<RoundedCornerElement>,
    Layer=WaylandSurfaceRenderElement<GlesRenderer>,
    Cursor=MemoryRenderBufferRenderElement<GlesRenderer>,
    CursorSurface=smithay::backend::renderer::element::Wrap<WaylandSurfaceRenderElement<GlesRenderer>>,
    Blur=TextureRenderElement<GlesTexture>,
}

// Shadow and Decoration share inner types with Background and Tile respectively.
// We can't add them to render_elements! because it generates conflicting From impls.
// Instead we construct them directly using the existing Background/Tile variants.
// Helpers below create the elements and wrap them in the correct variant.

/// Uniform declarations for background shaders.
/// Shaders receive only u_camera — zoom is handled externally via RescaleRenderElement.
pub const BG_UNIFORMS: &[UniformName<'static>] = &[UniformName {
    name: std::borrow::Cow::Borrowed("u_camera"),
    type_: UniformType::_2f,
}];

/// Shadow shader source — soft box-shadow around SSD windows.
const SHADOW_SHADER_SRC: &str = include_str!("shaders/shadow.glsl");

/// Uniform declarations for the shadow shader.
pub const SHADOW_UNIFORMS: &[UniformName<'static>] = &[
    UniformName {
        name: std::borrow::Cow::Borrowed("u_window_rect"),
        type_: UniformType::_4f,
    },
    UniformName {
        name: std::borrow::Cow::Borrowed("u_radius"),
        type_: UniformType::_1f,
    },
    UniformName {
        name: std::borrow::Cow::Borrowed("u_color"),
        type_: UniformType::_4f,
    },
    UniformName {
        name: std::borrow::Cow::Borrowed("u_corner_radius"),
        type_: UniformType::_1f,
    },
];

/// Compile the shadow shader program. Called once at startup alongside the background shader.
pub fn compile_shadow_shader(renderer: &mut GlesRenderer) -> Option<smithay::backend::renderer::gles::GlesPixelProgram> {
    match renderer.compile_custom_pixel_shader(SHADOW_SHADER_SRC, SHADOW_UNIFORMS) {
        Ok(shader) => Some(shader),
        Err(e) => {
            tracing::error!("Failed to compile shadow shader: {e}");
            None
        }
    }
}

fn shadow_uniforms(
    shadow_padding: i32,
    content_w: i32,
    content_h: i32,
    shadow_radius: f32,
    corner_radius: f32,
) -> Vec<Uniform<'static>> {
    use driftwm::config::DecorationConfig;
    let sc = DecorationConfig::SHADOW_COLOR;
    vec![
        Uniform::new("u_window_rect", (
            shadow_padding as f32, shadow_padding as f32,
            content_w as f32, content_h as f32,
        )),
        Uniform::new("u_radius", shadow_radius),
        Uniform::new("u_color", (
            sc[0] as f32 / 255.0, sc[1] as f32 / 255.0,
            sc[2] as f32 / 255.0, sc[3] as f32 / 255.0,
        )),
        Uniform::new("u_corner_radius", corner_radius),
    ]
}

const CORNER_CLIP_SRC: &str = include_str!("shaders/corner_clip.glsl");

pub const CORNER_CLIP_UNIFORMS: &[UniformName<'static>] = &[
    UniformName { name: Cow::Borrowed("u_size"), type_: UniformType::_2f },
    UniformName { name: Cow::Borrowed("u_geo"), type_: UniformType::_4f },
    UniformName { name: Cow::Borrowed("u_radius"), type_: UniformType::_1f },
    UniformName { name: Cow::Borrowed("u_clip_top"), type_: UniformType::_1f },
    UniformName { name: Cow::Borrowed("u_clip_shadow"), type_: UniformType::_1f },
];

pub fn compile_corner_clip_shader(renderer: &mut GlesRenderer) -> Option<GlesTexProgram> {
    match renderer.compile_custom_texture_shader(CORNER_CLIP_SRC, CORNER_CLIP_UNIFORMS) {
        Ok(shader) => Some(shader),
        Err(e) => {
            tracing::error!("Failed to compile corner clip shader: {e}");
            None
        }
    }
}

const TILE_BG_SRC: &str = include_str!("shaders/tile_bg.glsl");

pub const TILE_BG_UNIFORMS: &[UniformName<'static>] = &[
    UniformName { name: Cow::Borrowed("u_camera"), type_: UniformType::_2f },
    UniformName { name: Cow::Borrowed("u_tile_size"), type_: UniformType::_2f },
    UniformName { name: Cow::Borrowed("u_output_size"), type_: UniformType::_2f },
];

pub fn compile_tile_bg_shader(renderer: &mut GlesRenderer) -> Option<GlesTexProgram> {
    match renderer.compile_custom_texture_shader(TILE_BG_SRC, TILE_BG_UNIFORMS) {
        Ok(shader) => Some(shader),
        Err(e) => {
            tracing::error!("Failed to compile tile background shader: {e}");
            None
        }
    }
}

/// Wrapper element that applies a rounded-corner clipping shader to a window's root surface.
pub struct RoundedCornerElement {
    inner: WaylandSurfaceRenderElement<GlesRenderer>,
    shader: GlesTexProgram,
    uniforms: Vec<Uniform<'static>>,
    corner_radius: f64,
    clip_top: bool,
}

impl RoundedCornerElement {
    pub fn new(
        inner: WaylandSurfaceRenderElement<GlesRenderer>,
        shader: GlesTexProgram,
        uniforms: Vec<Uniform<'static>>,
        corner_radius: f64,
        clip_top: bool,
    ) -> Self {
        Self { inner, shader, uniforms, corner_radius, clip_top }
    }
}

impl Element for RoundedCornerElement {
    fn id(&self) -> &smithay::backend::renderer::element::Id { self.inner.id() }
    fn current_commit(&self) -> CommitCounter { self.inner.current_commit() }
    fn location(&self, scale: Scale<f64>) -> Point<i32, Physical> { self.inner.location(scale) }
    fn src(&self) -> Rectangle<f64, smithay::utils::Buffer> { self.inner.src() }
    fn transform(&self) -> Transform { self.inner.transform() }
    fn geometry(&self, scale: Scale<f64>) -> Rectangle<i32, Physical> { self.inner.geometry(scale) }
    fn damage_since(
        &self, scale: Scale<f64>, commit: Option<CommitCounter>,
    ) -> DamageSet<i32, Physical> {
        self.inner.damage_since(scale, commit)
    }
    fn opaque_regions(
        &self, scale: Scale<f64>,
    ) -> OpaqueRegions<i32, Physical> {
        let regions = self.inner.opaque_regions(scale);
        if regions.is_empty() || self.corner_radius <= 0.0 {
            return regions;
        }
        let geo = self.geometry(scale);
        // +1 to cover anti-aliased fringe from smoothstep
        let r = (self.corner_radius * scale.x).ceil() as i32 + 1;
        let (w, h) = (geo.size.w, geo.size.h);
        if w <= 2 * r || h <= 2 * r {
            return regions;
        }
        let mut corners = Vec::with_capacity(4);
        if self.clip_top {
            corners.push(Rectangle::new((0, 0).into(), (r, r).into()));
            corners.push(Rectangle::new((w - r, 0).into(), (r, r).into()));
        }
        corners.push(Rectangle::new((0, h - r).into(), (r, r).into()));
        corners.push(Rectangle::new((w - r, h - r).into(), (r, r).into()));
        let rects: Vec<_> = regions.into_iter().collect();
        Rectangle::subtract_rects_many_in_place(rects, corners).into_iter().collect()
    }
    fn alpha(&self) -> f32 { self.inner.alpha() }
    fn kind(&self) -> Kind { self.inner.kind() }
}

impl RenderElement<GlesRenderer> for RoundedCornerElement {
    fn draw(
        &self,
        frame: &mut GlesFrame<'_, '_>,
        src: Rectangle<f64, smithay::utils::Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        opaque_regions: &[Rectangle<i32, Physical>],
    ) -> Result<(), GlesError> {
        frame.override_default_tex_program(self.shader.clone(), self.uniforms.clone());
        let result = self.inner.draw(frame, src, dst, damage, opaque_regions);
        frame.clear_tex_program_override();
        result
    }

    fn underlying_storage(
        &self, renderer: &mut GlesRenderer,
    ) -> Option<smithay::backend::renderer::element::UnderlyingStorage<'_>> {
        self.inner.underlying_storage(renderer)
    }
}

// ── Blur infrastructure ─────────────────────────────────────────────

static BLUR_DOWN_SRC: &str = include_str!("shaders/blur_down.glsl");
static BLUR_UP_SRC: &str = include_str!("shaders/blur_up.glsl");

/// Per-window cached textures for Kawase blur ping-pong passes.
pub struct BlurCache {
    pub texture: GlesTexture,
    pub scratch: GlesTexture,
    pub mask: GlesTexture,
    pub size: Size<i32, Physical>,
    pub dirty: bool,
    pub last_scene_generation: u64,
    pub last_geometry_generation: u64,
    pub last_camera_generation: u64,
}

impl BlurCache {
    pub fn new(renderer: &mut GlesRenderer, size: Size<i32, Physical>) -> Option<Self> {
        use smithay::backend::renderer::Offscreen;
        let buf_size = size.to_logical(1).to_buffer(1, Transform::Normal);
        let t1 = Offscreen::<GlesTexture>::create_buffer(renderer, Fourcc::Abgr8888, buf_size).ok()?;
        let t2 = Offscreen::<GlesTexture>::create_buffer(renderer, Fourcc::Abgr8888, buf_size).ok()?;
        let t3 = Offscreen::<GlesTexture>::create_buffer(renderer, Fourcc::Abgr8888, buf_size).ok()?;
        Some(Self {
            texture: t1, scratch: t2, mask: t3, size,
            dirty: true, last_scene_generation: 0,
            last_geometry_generation: 0, last_camera_generation: 0,
        })
    }

    pub fn resize(&mut self, renderer: &mut GlesRenderer, size: Size<i32, Physical>) {
        use smithay::backend::renderer::Offscreen;
        let buf_size = size.to_logical(1).to_buffer(1, Transform::Normal);
        if let Ok(t1) = Offscreen::<GlesTexture>::create_buffer(renderer, Fourcc::Abgr8888, buf_size)
            && let Ok(t2) = Offscreen::<GlesTexture>::create_buffer(renderer, Fourcc::Abgr8888, buf_size)
            && let Ok(t3) = Offscreen::<GlesTexture>::create_buffer(renderer, Fourcc::Abgr8888, buf_size)
        {
            self.texture = t1;
            self.scratch = t2;
            self.mask = t3;
            self.size = size;
            self.dirty = true;
        }
    }
}

static BLUR_MASK_SRC: &str = include_str!("shaders/blur_mask.glsl");

pub fn compile_blur_shaders(renderer: &mut GlesRenderer) -> (Option<GlesTexProgram>, Option<GlesTexProgram>, Option<GlesTexProgram>) {
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

    let tex_size = tex_a.size();

    for i in 0..passes {
        blur_pass(renderer, down_shader, tex_a, tex_b, tex_size, offset, i, passes, true)?;
        std::mem::swap(tex_a, tex_b);
    }

    for i in 0..passes {
        blur_pass(renderer, up_shader, tex_a, tex_b, tex_size, offset, i, passes, false)?;
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
    tex_size: Size<i32, smithay::utils::Buffer>,
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

    let src_w = (tex_size.w >> src_shift).max(1);
    let src_h = (tex_size.h >> src_shift).max(1);
    let dst_w = (tex_size.w >> dst_shift).max(1);
    let dst_h = (tex_size.h >> dst_shift).max(1);

    let half_pixel = [1.0 / src_w as f32, 1.0 / src_h as f32];
    let pass_offset = offset / (1 << src_shift) as f32;

    let dst_phys: Size<i32, Physical> = (dst_w, dst_h).into();
    let src_buf: Rectangle<f64, smithay::utils::Buffer> =
        Rectangle::from_size((src_w as f64, src_h as f64).into());

    let src = tex_a.clone();
    {
        let mut target = renderer.bind(tex_b)?;
        let mut frame = renderer.render(&mut target, dst_phys, Transform::Normal)?;
        frame.clear(
            Color32F::TRANSPARENT,
            &[Rectangle::from_size(dst_phys)],
        )?;
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

/// Build render elements for X11 override-redirect windows (menus, tooltips, splashes).
/// Same camera/zoom math as managed windows.
fn build_override_redirect_elements(
    state: &crate::state::DriftWm,
    renderer: &mut GlesRenderer,
    output: &Output,
    camera: Point<f64, Logical>,
    zoom: f64,
) -> Vec<OutputRenderElements> {
    let output_scale = output.current_scale().fractional_scale();
    let scale = Scale::from(output_scale);
    let viewport_size = crate::state::output_logical_size(output);
    let visible_rect = canvas::visible_canvas_rect(camera.to_i32_round(), viewport_size, zoom);

    let mut elements = Vec::new();
    // Reverse: newest OR window = topmost
    for or_surface in state.x11_override_redirect.iter().rev() {
        let Some(wl_surface) = or_surface.wl_surface() else { continue };
        let canvas_pos = state.or_canvas_position(or_surface);
        let or_size = or_surface.geometry().size;
        let or_rect = Rectangle::new(canvas_pos, or_size);
        if !visible_rect.overlaps(or_rect) { continue }

        let render_loc: Point<f64, Logical> = Point::from((
            canvas_pos.x as f64 - camera.x,
            canvas_pos.y as f64 - camera.y,
        ));
        let physical_loc: Point<f64, Physical> = render_loc.to_physical_precise_round(scale);
        let elems: Vec<WaylandSurfaceRenderElement<GlesRenderer>> =
            smithay::backend::renderer::element::surface::render_elements_from_surface_tree(
                renderer,
                &wl_surface,
                physical_loc.to_i32_round(),
                scale,
                1.0,
                Kind::Unspecified,
            );
        elements.extend(elems.into_iter().map(|elem| {
            OutputRenderElements::Window(RescaleRenderElement::from_element(
                elem,
                Point::<i32, Physical>::from((0, 0)),
                zoom,
            ))
        }));
    }
    elements
}

/// Build render elements for canvas-positioned layer surfaces (zoomed like windows).
/// Mirrors the window pipeline: position relative to camera, then RescaleRenderElement for zoom.
pub fn build_canvas_layer_elements(
    state: &crate::state::DriftWm,
    renderer: &mut GlesRenderer,
    output: &Output,
    camera: Point<f64, smithay::utils::Logical>,
    zoom: f64,
) -> Vec<OutputRenderElements> {
    let output_scale = output.current_scale().fractional_scale();
    let mut elements = Vec::new();

    for cl in &state.canvas_layers {
        let Some(pos) = cl.position else { continue; };
        // Camera-relative position (same as render_elements_for_region does for windows)
        let rel: Point<f64, Logical> = Point::from((
            pos.x as f64 - camera.x,
            pos.y as f64 - camera.y,
        ));
        let physical_loc = rel.to_physical_precise_round(output_scale);

        let surface_elements = cl
            .surface
            .render_elements::<WaylandSurfaceRenderElement<GlesRenderer>>(
                renderer,
                physical_loc,
                smithay::utils::Scale::from(output_scale),
                1.0,
            );
        elements.extend(surface_elements.into_iter().map(|elem| {
            OutputRenderElements::Window(RescaleRenderElement::from_element(
                elem,
                Point::<i32, Physical>::from((0, 0)),
                zoom,
            ))
        }));
    }

    elements
}

/// Build render elements for all layer surfaces on the given layer.
/// Layer surfaces are screen-fixed (not zoomed), so they use raw WaylandSurfaceRenderElement.
///
/// When `blur_config` is `Some`, layer surfaces whose `namespace()` matches a window rule
/// with `blur = true` will produce `BlurRequestData` entries alongside their render elements.
fn build_layer_elements(
    output: &Output,
    renderer: &mut GlesRenderer,
    layer: WlrLayer,
    blur_config: Option<(&driftwm::config::Config, bool, BlurLayer)>,
) -> (Vec<OutputRenderElements>, Vec<BlurRequestData>) {
    let map = layer_map_for_output(output);
    let output_scale = output.current_scale().fractional_scale();
    let mut elements = Vec::new();
    let mut blur_requests = Vec::new();

    for surface in map.layers_on(layer).rev() {
        let geo = map.layer_geometry(surface).unwrap_or_default();
        let loc = geo.loc.to_physical_precise_round(output_scale);

        let elem_start = elements.len();
        elements.extend(
            surface
                .render_elements::<WaylandSurfaceRenderElement<GlesRenderer>>(
                    renderer,
                    loc,
                    smithay::utils::Scale::from(output_scale),
                    1.0,
                )
                .into_iter()
                .map(OutputRenderElements::Layer),
        );

        if let Some((config, blur_enabled, layer_tag)) = blur_config
            && blur_enabled
            && config.match_window_rule(surface.namespace(), "").is_some_and(|r| r.blur)
        {
            let elem_count = elements.len() - elem_start;
            let screen_rect = geo.to_physical_precise_round(output_scale);
            blur_requests.push(BlurRequestData {
                surface_id: surface.wl_surface().id(),
                screen_rect,
                elem_start,
                elem_count,
                layer: layer_tag,
            });
        }
    }

    (elements, blur_requests)
}

/// Resolve which xcursor name to load for the current cursor status.
/// Build the cursor render element(s) for the current frame.
/// `camera` and `zoom` are from the output being rendered.
/// Returns `OutputRenderElements` — either xcursor memory buffers or client surface elements.
pub fn build_cursor_elements(
    state: &mut crate::state::DriftWm,
    renderer: &mut GlesRenderer,
    camera: Point<f64, smithay::utils::Logical>,
    zoom: f64,
    alpha: f32,
) -> Vec<OutputRenderElements> {
    if alpha <= 0.0 {
        return vec![];
    }
    let pointer = state.seat.get_pointer().unwrap();
    let canvas_pos = pointer.current_location();
    // Custom elements are in screen-local physical coords
    let screen_pos = canvas_to_screen(CanvasPos(canvas_pos), camera, zoom).0;
    let physical_pos: Point<f64, Physical> = (screen_pos.x, screen_pos.y).into();

    // Separate the status check from mutable state access (Rust 2024 borrow rules)
    let status = state.cursor.cursor_status.clone();
    match status {
        CursorImageStatus::Hidden => vec![],
        CursorImageStatus::Surface(ref surface) => {
            if !surface.alive() {
                state.cursor.cursor_status = CursorImageStatus::default_named();
                return build_xcursor_elements(state, renderer, physical_pos, "default", alpha);
            }
            let hotspot = with_states(surface, |states| {
                states
                    .data_map
                    .get::<CursorImageSurfaceData>()
                    .map(|d| d.lock().unwrap().hotspot)
                    .unwrap_or_default()
            });
            let pos: Point<i32, Physical> = (
                (physical_pos.x - hotspot.x as f64) as i32,
                (physical_pos.y - hotspot.y as f64) as i32,
            ).into();
            let elems: Vec<WaylandSurfaceRenderElement<GlesRenderer>> =
                smithay::backend::renderer::element::surface::render_elements_from_surface_tree(
                    renderer,
                    surface,
                    pos,
                    Scale::from(1.0),
                    alpha,
                    Kind::Cursor,
                );
            elems.into_iter().map(|e| OutputRenderElements::CursorSurface(e.into())).collect()
        }
        CursorImageStatus::Named(icon) => {
            build_xcursor_elements(state, renderer, physical_pos, icon.name(), alpha)
        }
    }
}

/// Build xcursor memory buffer elements for a named cursor icon.
fn build_xcursor_elements(
    state: &mut crate::state::DriftWm,
    renderer: &mut GlesRenderer,
    physical_pos: Point<f64, Physical>,
    name: &'static str,
    alpha: f32,
) -> Vec<OutputRenderElements> {
    let loaded = state.load_xcursor(name).is_some();
    if !loaded && state.load_xcursor("default").is_none() {
        return vec![];
    }
    let key = if loaded { name } else { "default" };
    let cursor_frames = state.cursor.cursor_buffers.get(key).unwrap();

    // Select the active frame
    let frame_idx = if cursor_frames.total_duration_ms == 0 {
        0
    } else {
        let elapsed = state.start_time.elapsed().as_millis() as u32
            % cursor_frames.total_duration_ms;
        let mut acc = 0u32;
        let mut idx = 0;
        for (i, &(_, _, delay)) in cursor_frames.frames.iter().enumerate() {
            acc += delay;
            if elapsed < acc {
                idx = i;
                break;
            }
        }
        idx
    };

    let (buffer, hotspot, _) = &cursor_frames.frames[frame_idx];
    let hotspot = *hotspot;

    let pos = physical_pos - Point::from((hotspot.x as f64, hotspot.y as f64));
    match MemoryRenderBufferRenderElement::from_buffer(
        renderer,
        pos,
        buffer,
        Some(alpha),
        None,
        None,
        Kind::Cursor,
    ) {
        Ok(elem) => vec![OutputRenderElements::Cursor(elem)],
        Err(_) => vec![],
    }
}

/// Update the cached background shader element for the current camera/zoom.
/// Returns (camera_moved, zoom_changed) for the caller's damage logic.
pub fn update_background_element(
    state: &mut crate::state::DriftWm,
    output: &Output,
    cur_camera: Point<f64, smithay::utils::Logical>,
    cur_zoom: f64,
    last_rendered_camera: Point<f64, smithay::utils::Logical>,
    last_rendered_zoom: f64,
) -> (bool, bool) {
    let camera_moved = cur_camera != last_rendered_camera;
    let zoom_changed = cur_zoom != last_rendered_zoom;
    let output_name = output.name();
    let scale = output.current_scale().integer_scale();
    let output_size = output
        .current_mode()
        .map(|m| output.current_transform().transform_size(m.size.to_logical(scale)))
        .unwrap_or((1, 1).into());
    let canvas_w = (output_size.w as f64 / cur_zoom).ceil() as i32;
    let canvas_h = (output_size.h as f64 / cur_zoom).ceil() as i32;
    let canvas_area = Rectangle::from_size((canvas_w, canvas_h).into());

    if let Some(elem) = state.render.cached_bg_elements.get_mut(&output_name) {
        elem.resize(canvas_area, Some(vec![canvas_area]));
        elem.update_uniforms(vec![Uniform::new(
            "u_camera",
            (cur_camera.x as f32, cur_camera.y as f32),
        )]);
    } else if let Some(elem) = state.render.cached_tile_bg.get_mut(&output_name) {
        elem.resize(canvas_area, Some(vec![canvas_area]));
        elem.update_uniforms(vec![
            Uniform::new("u_camera", (cur_camera.x as f32, cur_camera.y as f32)),
            Uniform::new("u_tile_size", (elem.tex_w as f32, elem.tex_h as f32)),
            Uniform::new("u_output_size", (canvas_w as f32, canvas_h as f32)),
        ]);
    }
    (camera_moved, zoom_changed)
}

/// Build render elements for a locked session: only the lock surface.
/// No compositor cursor — the lock client manages its own visuals.
fn compose_lock_frame(
    state: &crate::state::DriftWm,
    renderer: &mut GlesRenderer,
    output: &Output,
    _cursor_elements: Vec<OutputRenderElements>,
) -> Vec<OutputRenderElements> {
    let mut elements = Vec::new();

    if let Some(lock_surface) = state.lock_surfaces.get(output) {
        let output_scale = output.current_scale().fractional_scale();
        let lock_elements = smithay::backend::renderer::element::surface::render_elements_from_surface_tree(
            renderer,
            lock_surface.wl_surface(),
            (0, 0),
            Scale::from(output_scale),
            1.0,
            Kind::Unspecified,
        );
        elements.extend(lock_elements.into_iter().map(OutputRenderElements::Layer));
    }

    elements
}

/// Assemble all render elements for a frame.
/// Caller provides cursor elements (built before taking the renderer).
pub fn compose_frame(
    state: &mut crate::state::DriftWm,
    renderer: &mut GlesRenderer,
    output: &Output,
    cursor_elements: Vec<OutputRenderElements>,
) -> Vec<OutputRenderElements> {
    // Session lock: render only lock surface (or black) + cursor
    if !matches!(state.session_lock, crate::state::SessionLock::Unlocked) {
        return compose_lock_frame(state, renderer, output, cursor_elements);
    }

    // Ensure this output has a background element (lazy init per output, and re-init after config reload)
    if !state.render.cached_bg_elements.contains_key(&output.name()) && !state.render.cached_tile_bg.contains_key(&output.name()) {
        let output_size = output
            .current_mode()
            .map(|m| output.current_transform().transform_size(
                m.size.to_logical(output.current_scale().integer_scale()),
            ))
            .unwrap_or((1, 1).into());
        init_background(state, renderer, output_size, &output.name());
    }

    // Read per-output state directly — not via active_output() which follows the pointer
    let (camera, zoom) = {
        let os = crate::state::output_state(output);
        (os.camera, os.zoom)
    };

    let viewport_size = crate::state::output_logical_size(output);
    let visible_rect = canvas::visible_canvas_rect(
        camera.to_i32_round(),
        viewport_size,
        zoom,
    );
    let output_scale = output.current_scale().fractional_scale();
    let scale = Scale::from(output_scale);

    // Split windows into normal and widget layers so canvas layers render between them.
    // Replicates render_elements_for_region internals: bbox overlap, camera offset, zoom.
    let mut zoomed_normal: Vec<OutputRenderElements> = Vec::new();
    let mut zoomed_widgets: Vec<OutputRenderElements> = Vec::new();

    let blur_enabled = state.render.blur_down_shader.is_some() && state.render.blur_up_shader.is_some() && state.render.blur_mask_shader.is_some();
    let mut blur_requests: Vec<BlurRequestData> = Vec::new();

    // Focused surface for decoration focus state
    let focused_surface = state
        .seat
        .get_keyboard()
        .and_then(|kb| kb.current_focus())
        .map(|f| f.0);

    for window in state.space.elements().rev() {
        let Some(loc) = state.space.element_location(window) else { continue };
        let geom_loc = window.geometry().loc;
        let geom_size = window.geometry().size;
        let Some(wl_surface) = window.wl_surface() else { continue; };
        let is_fullscreen = state.fullscreen.values().any(|fs| &fs.window == window);
        let has_ssd = !is_fullscreen && state.decorations.contains_key(&wl_surface.id());

        let mut bbox = window.bbox();
        bbox.loc += loc - geom_loc;
        if has_ssd {
            let r = driftwm::config::DecorationConfig::SHADOW_RADIUS.ceil() as i32;
            let bar = driftwm::config::DecorationConfig::TITLE_BAR_HEIGHT;
            bbox.loc.x -= r;
            bbox.loc.y -= bar + r;
            bbox.size.w += 2 * r;
            bbox.size.h += bar + 2 * r;
        }
        if !visible_rect.overlaps(bbox) { continue }

        let render_loc: Point<f64, Logical> = Point::from((
            loc.x as f64 - geom_loc.x as f64 - camera.x,
            loc.y as f64 - geom_loc.y as f64 - camera.y,
        ));
        let applied = driftwm::config::applied_rule(&wl_surface);
        let is_widget = applied.as_ref().is_some_and(|r| r.widget);
        let wants_blur = blur_enabled && applied.as_ref().is_some_and(|r| r.blur);
        let opacity = applied.as_ref().and_then(|r| r.opacity).unwrap_or(1.0);

        let elems = window.render_elements::<WaylandSurfaceRenderElement<GlesRenderer>>(
            renderer,
            render_loc.to_physical_precise_round(scale),
            scale,
            opacity as f32,
        );

        let target = if is_widget { &mut zoomed_widgets } else { &mut zoomed_normal };
        let elem_start = target.len();
        let mut shadow_count = 0usize;

        if has_ssd {
            let bar_height = driftwm::config::DecorationConfig::TITLE_BAR_HEIGHT;
            let is_focused = focused_surface.as_ref().is_some_and(|f| *f == *wl_surface);

            // Update decoration state (re-render title bar if needed)
            if let Some(deco) = state.decorations.get_mut(&wl_surface.id()) {
                deco.update(geom_size.w, is_focused, &state.config.decorations);
            }

            // Title bar element: positioned above the window
            if let Some(deco) = state.decorations.get(&wl_surface.id()) {
                let bar_loc: Point<f64, Logical> = Point::from((
                    render_loc.x,
                    render_loc.y - bar_height as f64,
                ));
                let bar_physical: Point<f64, Physical> = bar_loc.to_physical_precise_round(scale);
                let bar_alpha = if opacity < 1.0 { Some(opacity as f32) } else { None };
                if let Ok(bar_elem) = MemoryRenderBufferRenderElement::from_buffer(
                    renderer,
                    bar_physical,
                    &deco.title_bar,
                    bar_alpha,
                    None,
                    None,
                    Kind::Unspecified,
                ) {
                    target.push(OutputRenderElements::Decoration(
                        RescaleRenderElement::from_element(
                            bar_elem,
                            Point::<i32, Physical>::from((0, 0)),
                            zoom,
                        ),
                    ));
                }
            }

            // Window surface elements — clip bottom corners to match title bar rounding
            if let Some(ref shader) = state.render.corner_clip_shader {
                let radius = state.config.decorations.corner_radius as f32;
                if radius > 0.0 {
                    let toplevel_id = smithay::backend::renderer::element::Id::from_wayland_resource(&*wl_surface);
                    for elem in elems {
                        if *elem.id() == toplevel_id {
                            let buf = elem.buffer_size();
                            // SSD windows: geometry is the full buffer, only clip bottom corners
                            let uniforms = vec![
                                Uniform::new("u_size", (buf.w as f32, buf.h as f32)),
                                Uniform::new("u_geo", (0.0f32, 0.0f32, buf.w as f32, buf.h as f32)),
                                Uniform::new("u_radius", radius),
                                Uniform::new("u_clip_top", 0.0f32),
                                Uniform::new("u_clip_shadow", 0.0f32),
                            ];
                            target.push(OutputRenderElements::CsdWindow(RescaleRenderElement::from_element(
                                RoundedCornerElement::new(elem, shader.clone(), uniforms, radius as f64, false),
                                Point::<i32, Physical>::from((0, 0)),
                                zoom,
                            )));
                        } else {
                            target.push(OutputRenderElements::Window(RescaleRenderElement::from_element(
                                elem,
                                Point::<i32, Physical>::from((0, 0)),
                                zoom,
                            )));
                        }
                    }
                } else {
                    target.extend(elems.into_iter().map(|elem| {
                        OutputRenderElements::Window(RescaleRenderElement::from_element(
                            elem,
                            Point::<i32, Physical>::from((0, 0)),
                            zoom,
                        ))
                    }));
                }
            } else {
                target.extend(elems.into_iter().map(|elem| {
                    OutputRenderElements::Window(RescaleRenderElement::from_element(
                        elem,
                        Point::<i32, Physical>::from((0, 0)),
                        zoom,
                    ))
                }));
            }

            // Shadow element: cached per-window, rebuilt only on resize.
            // Stable Id lets the damage tracker skip unchanged shadow regions.
            if let Some(ref shader) = state.render.shadow_shader {
                use driftwm::config::DecorationConfig;
                let radius = DecorationConfig::SHADOW_RADIUS;
                let r = radius.ceil() as i32;
                let shadow_w = geom_size.w + 2 * r;
                let shadow_h = geom_size.h + bar_height + 2 * r;
                let shadow_loc: Point<i32, Logical> = Point::from((
                    render_loc.x.round() as i32 - r,
                    render_loc.y.round() as i32 - bar_height - r,
                ));
                let shadow_area = Rectangle::new(shadow_loc, (shadow_w, shadow_h).into());
                let corner_r = state.config.decorations.corner_radius as f32;

                if let Some(deco) = state.decorations.get_mut(&wl_surface.id()) {
                    let content_size = (geom_size.w, geom_size.h);
                    if deco.cached_shadow.as_ref().is_some_and(|s| (s.alpha() - opacity as f32).abs() > f32::EPSILON) {
                        deco.cached_shadow = None;
                    }
                    let shadow_elem = if let Some(shadow) = &mut deco.cached_shadow {
                        if deco.shadow_content_size != content_size {
                            deco.shadow_content_size = content_size;
                            shadow.update_uniforms(shadow_uniforms(
                                r, geom_size.w, geom_size.h + bar_height, radius, corner_r,
                            ));
                        }
                        shadow.resize(shadow_area, None);
                        shadow.clone()
                    } else {
                        deco.shadow_content_size = content_size;
                        let elem = PixelShaderElement::new(
                            shader.clone(),
                            shadow_area,
                            None,
                            opacity as f32,
                            shadow_uniforms(r, geom_size.w, geom_size.h + bar_height, radius, corner_r),
                            Kind::Unspecified,
                        );
                        deco.cached_shadow = Some(elem.clone());
                        elem
                    };
                    target.push(OutputRenderElements::Background(
                        RescaleRenderElement::from_element(
                            shadow_elem,
                            Point::<i32, Physical>::from((0, 0)),
                            zoom,
                        ),
                    ));
                    shadow_count = 1;
                }
            }
        } else if let Some(ref shader) = state.render.corner_clip_shader {
            let geo = window.geometry();
            let radius = state.config.decorations.corner_radius as f32;

            let rule_forced = applied.as_ref().is_some_and(|r| {
                r.decoration != driftwm::config::DecorationMode::Client
            });

            if !rule_forced {
                if radius > 0.0 {
                    let toplevel_id = smithay::backend::renderer::element::Id::from_wayland_resource(&*wl_surface);
                    for elem in elems {
                        if *elem.id() == toplevel_id {
                            let buf = elem.buffer_size();
                            let uniforms = vec![
                                Uniform::new("u_size", (buf.w as f32, buf.h as f32)),
                                Uniform::new("u_geo", (
                                    geo.loc.x as f32, geo.loc.y as f32,
                                    geo.size.w as f32, geo.size.h as f32,
                                )),
                                Uniform::new("u_radius", radius),
                                Uniform::new("u_clip_top", 1.0f32),
                                Uniform::new("u_clip_shadow", 1.0f32),
                            ];
                            target.push(OutputRenderElements::CsdWindow(RescaleRenderElement::from_element(
                                RoundedCornerElement::new(elem, shader.clone(), uniforms, radius as f64, true),
                                Point::<i32, Physical>::from((0, 0)),
                                zoom,
                            )));
                        } else {
                            target.push(OutputRenderElements::Window(RescaleRenderElement::from_element(
                                elem,
                                Point::<i32, Physical>::from((0, 0)),
                                zoom,
                            )));
                        }
                    }
                } else {
                    target.extend(elems.into_iter().map(|elem| {
                        OutputRenderElements::Window(RescaleRenderElement::from_element(
                            elem,
                            Point::<i32, Physical>::from((0, 0)),
                            zoom,
                        ))
                    }));
                }

                // Compositor shadow behind CSD windows
                if let Some(ref shadow_shader) = state.render.shadow_shader {
                    use driftwm::config::DecorationConfig;
                    let shadow_radius = DecorationConfig::SHADOW_RADIUS;
                    let sr = shadow_radius.ceil() as i32;
                    let shadow_w = geom_size.w + 2 * sr;
                    let shadow_h = geom_size.h + 2 * sr;
                    // render_loc is the buffer origin; geometry starts at render_loc + geo.loc
                    let shadow_loc: Point<i32, Logical> = Point::from((
                        render_loc.x.round() as i32 + geo.loc.x - sr,
                        render_loc.y.round() as i32 + geo.loc.y - sr,
                    ));
                    let shadow_area = Rectangle::new(shadow_loc, (shadow_w, shadow_h).into());
                    let content_size = (geom_size.w, geom_size.h);
                    let corner_r = state.config.decorations.corner_radius as f32;

                    let shadow_entry = state.render.csd_shadows.entry(wl_surface.id());
                    let (shadow_elem, cached_size) = shadow_entry.or_insert_with(|| {
                        let elem = PixelShaderElement::new(
                            shadow_shader.clone(),
                            shadow_area,
                            None,
                            opacity as f32,
                            shadow_uniforms(sr, geom_size.w, geom_size.h, shadow_radius, corner_r),
                            Kind::Unspecified,
                        );
                        (elem, content_size)
                    });

                    if *cached_size != content_size {
                        *cached_size = content_size;
                        shadow_elem.update_uniforms(shadow_uniforms(
                            sr, geom_size.w, geom_size.h, shadow_radius, corner_r,
                        ));
                    }
                    shadow_elem.resize(shadow_area, None);
                    target.push(OutputRenderElements::Background(
                        RescaleRenderElement::from_element(
                            shadow_elem.clone(),
                            Point::<i32, Physical>::from((0, 0)),
                            zoom,
                        ),
                    ));
                    shadow_count = 1;
                }
            } else {
                target.extend(elems.into_iter().map(|elem| {
                    OutputRenderElements::Window(RescaleRenderElement::from_element(
                        elem,
                        Point::<i32, Physical>::from((0, 0)),
                        zoom,
                    ))
                }));
            }
        } else {
            target.extend(elems.into_iter().map(|elem| {
                OutputRenderElements::Window(RescaleRenderElement::from_element(
                    elem,
                    Point::<i32, Physical>::from((0, 0)),
                    zoom,
                ))
            }));
        }

        if wants_blur {
            let elem_count = target.len() - elem_start - shadow_count;
            let screen_loc: Point<i32, Logical> = Point::from((
                (render_loc.x * zoom) as i32,
                (render_loc.y * zoom) as i32,
            ));
            let screen_size: Size<i32, Logical> = if has_ssd {
                let bar = driftwm::config::DecorationConfig::TITLE_BAR_HEIGHT;
                (
                    (geom_size.w as f64 * zoom).ceil() as i32,
                    ((geom_size.h + bar) as f64 * zoom).ceil() as i32,
                ).into()
            } else {
                (
                    (geom_size.w as f64 * zoom).ceil() as i32,
                    (geom_size.h as f64 * zoom).ceil() as i32,
                ).into()
            };
            let screen_rect = Rectangle::new(
                if has_ssd {
                    Point::from((
                        screen_loc.x,
                        screen_loc.y - (driftwm::config::DecorationConfig::TITLE_BAR_HEIGHT as f64 * zoom) as i32,
                    ))
                } else {
                    // CSD windows: geometry starts at render_loc + geo.loc, not at render_loc
                    let geo = window.geometry();
                    Point::from((
                        ((render_loc.x + geo.loc.x as f64) * zoom) as i32,
                        ((render_loc.y + geo.loc.y as f64) * zoom) as i32,
                    ))
                },
                screen_size,
            ).to_physical_precise_round(output_scale);
            blur_requests.push(BlurRequestData {
                surface_id: wl_surface.id(),
                screen_rect,
                elem_start,
                elem_count,
                layer: if is_widget { BlurLayer::Widget } else { BlurLayer::Normal },
            });
        }
    }

    let canvas_layer_elements = build_canvas_layer_elements(state, renderer, output, camera, zoom);

    let or_elements = build_override_redirect_elements(state, renderer, output, camera, zoom);

    let outline_elements = build_output_outline_elements(
        state, renderer, output, camera, zoom, viewport_size,
    );

    let bg_elements: Vec<OutputRenderElements> =
        if let Some(elem) = state.render.cached_bg_elements.get(&output.name()) {
            vec![OutputRenderElements::Background(
                RescaleRenderElement::from_element(
                    elem.clone(),
                    Point::<i32, Physical>::from((0, 0)),
                    zoom,
                ),
            )]
        } else if let Some(elem) = state.render.cached_tile_bg.get(&output.name()) {
            vec![OutputRenderElements::TileBg(
                RescaleRenderElement::from_element(
                    elem.clone(),
                    Point::<i32, Physical>::from((0, 0)),
                    zoom,
                ),
            )]
        } else {
            vec![]
        };

    let is_fullscreen = state.is_output_fullscreen(output);
    let (overlay_elements, overlay_blur) = build_layer_elements(
        output, renderer, WlrLayer::Overlay,
        Some((&state.config, blur_enabled, BlurLayer::Overlay)),
    );
    let (top_elements, top_blur) = if !is_fullscreen {
        build_layer_elements(
            output, renderer, WlrLayer::Top,
            Some((&state.config, blur_enabled, BlurLayer::Top)),
        )
    } else {
        (vec![], vec![])
    };
    let (bottom_elements, _) = if !is_fullscreen {
        build_layer_elements(output, renderer, WlrLayer::Bottom, None)
    } else {
        (vec![], vec![])
    };
    let (background_layer_elements, _) = build_layer_elements(output, renderer, WlrLayer::Background, None);

    // Compute prefix offsets so we know where each group lands in all_elements
    let overlay_prefix = cursor_elements.len() + or_elements.len();
    let top_prefix = overlay_prefix + overlay_elements.len();
    let normal_prefix = top_prefix + top_elements.len();
    let widget_prefix = normal_prefix
        + zoomed_normal.len()
        + canvas_layer_elements.len();

    // Merge blur requests: layer surfaces first (front-to-back), then windows
    let mut all_blur_requests: Vec<BlurRequestData> = Vec::new();
    all_blur_requests.extend(overlay_blur);
    all_blur_requests.extend(top_blur);
    all_blur_requests.extend(blur_requests);

    let mut all_elements: Vec<OutputRenderElements> = Vec::with_capacity(
        cursor_elements.len()
            + or_elements.len()
            + overlay_elements.len()
            + top_elements.len()
            + zoomed_normal.len()
            + canvas_layer_elements.len()
            + zoomed_widgets.len()
            + bottom_elements.len()
            + outline_elements.len()
            + bg_elements.len()
            + background_layer_elements.len(),
    );
    all_elements.extend(cursor_elements);
    all_elements.extend(or_elements);
    all_elements.extend(overlay_elements);
    all_elements.extend(top_elements);
    all_elements.extend(zoomed_normal);
    all_elements.extend(canvas_layer_elements);
    all_elements.extend(zoomed_widgets);
    all_elements.extend(bottom_elements);
    all_elements.extend(outline_elements);
    all_elements.extend(bg_elements);
    all_elements.extend(background_layer_elements);

    // Process blur requests: render behind-content, blur, insert
    if !all_blur_requests.is_empty() {
        process_blur_requests(
            state, renderer, output, output_scale,
            &mut all_elements, &all_blur_requests,
            overlay_prefix, top_prefix, normal_prefix, widget_prefix,
        );
    }

    // Prune stale blur cache entries
    if blur_enabled {
        let active_ids: std::collections::HashSet<_> =
            all_blur_requests.iter().map(|r| r.surface_id.clone()).collect();
        state.render.blur_cache.retain(|id, _| active_ids.contains(id));
    }

    all_elements
}

/// Process blur requests: for each blurred window, render behind-content to FBO,
/// crop the window region, run Kawase blur passes, and insert the result.
#[allow(clippy::too_many_arguments)]
fn process_blur_requests(
    state: &mut crate::state::DriftWm,
    renderer: &mut GlesRenderer,
    output: &Output,
    output_scale: f64,
    all_elements: &mut Vec<OutputRenderElements>,
    blur_requests: &[BlurRequestData],
    overlay_prefix: usize,
    top_prefix: usize,
    normal_prefix: usize,
    widget_prefix: usize,
) {
    use smithay::backend::renderer::{Bind, Frame, Offscreen, Renderer};
    use smithay::backend::renderer::Color32F;
    use smithay::backend::renderer::damage::OutputDamageTracker;
    use smithay::backend::renderer::element::Id;

    let output_size = output
        .current_mode()
        .map(|m| {
            let logical = output.current_transform().transform_size(
                m.size.to_logical(output.current_scale().integer_scale()),
            );
            logical.to_physical_precise_round(output_scale)
        })
        .unwrap_or(Size::from((1, 1)));

    let out_buf_size = output_size.to_logical(1).to_buffer(1, Transform::Normal);

    // Shared full-output FBO for behind-content rendering — cached on DriftWm, reused if size matches
    let mut bg_tex = match state.render.blur_bg_fbo.take() {
        Some((tex, cached_size)) if cached_size == output_size => tex,
        _ => {
            let Ok(t) = Offscreen::<GlesTexture>::create_buffer(renderer, Fourcc::Abgr8888, out_buf_size)
            else { return };
            t
        }
    };

    let down_shader = state.render.blur_down_shader.clone().unwrap();
    let up_shader = state.render.blur_up_shader.clone().unwrap();
    let blur_passes = state.config.effects.blur_radius as usize;
    let blur_strength = state.config.effects.blur_strength as f32;
    let context_id = renderer.context_id();
    let scene_gen = state.render.blur_scene_generation;
    let geom_gen = state.render.blur_geometry_generation;
    let camera_gen = state.render.blur_camera_generation;

    // ── First pass: create/resize caches, update dirty flags, decide who recomputes ──
    let mut needs_recompute: Vec<bool> = Vec::with_capacity(blur_requests.len());
    for req in blur_requests.iter() {
        let win_size = req.screen_rect.size;
        if win_size.w <= 0 || win_size.h <= 0 {
            needs_recompute.push(false);
            continue;
        }

        let is_new = !state.render.blur_cache.contains_key(&req.surface_id);
        if is_new {
            if let Some(c) = BlurCache::new(renderer, win_size) {
                state.render.blur_cache.insert(req.surface_id.clone(), c);
            } else {
                needs_recompute.push(false);
                continue;
            }
        }
        let cache = state.render.blur_cache.get_mut(&req.surface_id).unwrap();
        if cache.size != win_size {
            cache.resize(renderer, win_size);
        }

        let content_changed = cache.last_scene_generation != scene_gen;
        let geom_changed = cache.last_geometry_generation != geom_gen;
        // Layer surfaces are screen-fixed — camera pans scroll the canvas behind them
        let camera_dirty = matches!(req.layer, BlurLayer::Overlay | BlurLayer::Top)
            && cache.last_camera_generation != camera_gen;

        if content_changed || geom_changed || camera_dirty {
            cache.dirty = true;
        }
        cache.last_scene_generation = scene_gen;
        cache.last_geometry_generation = geom_gen;
        cache.last_camera_generation = camera_gen;

        needs_recompute.push(cache.dirty);
    }

    // Precompute per-request behind depth (index into all_elements where "below this window" begins)
    let behind_starts: Vec<usize> = blur_requests.iter().map(|req| {
        let prefix = match req.layer {
            BlurLayer::Overlay => overlay_prefix,
            BlurLayer::Top => top_prefix,
            BlurLayer::Normal => normal_prefix,
            BlurLayer::Widget => widget_prefix,
        };
        (prefix + req.elem_start + req.elem_count).min(all_elements.len())
    }).collect();

    let mask_shader = state.render.blur_mask_shader.clone();

    // ── Loop 1: re-render bg_tex per depth, crop + blur dirty windows ──
    // Requests are front-to-back so behind_start increases (each successive
    // bg render is a shorter suffix — cheaper). Re-render only when depth changes.
    let mut last_bg_depth: Option<usize> = None;
    for (i, req) in blur_requests.iter().enumerate() {
        if !needs_recompute[i] { continue; }
        let win_size = req.screen_rect.size;
        if win_size.w <= 0 || win_size.h <= 0 { continue; }
        let Some(cache) = state.render.blur_cache.get_mut(&req.surface_id) else { continue };

        let behind = behind_starts[i];
        if last_bg_depth != Some(behind) {
            let Ok(mut target) = renderer.bind(&mut bg_tex) else {
                state.render.blur_bg_fbo = Some((bg_tex, output_size));
                return;
            };
            let mut dt = OutputDamageTracker::new(output_size, output_scale, Transform::Normal);
            let _ = dt.render_output(
                renderer,
                &mut target,
                0,
                &all_elements[behind..],
                [0.0f32, 0.0, 0.0, 1.0],
            );
            last_bg_depth = Some(behind);
        }

        // Crop from bg_tex into cache.texture
        {
            let bg_src = bg_tex.clone();
            let Ok(mut target) = renderer.bind(&mut cache.texture) else { continue };
            let Ok(mut frame) = renderer.render(&mut target, win_size, Transform::Normal) else { continue };
            let _ = frame.clear(Color32F::TRANSPARENT, &[Rectangle::from_size(win_size)]);
            let src_rect: Rectangle<f64, smithay::utils::Buffer> = Rectangle::new(
                (req.screen_rect.loc.x as f64, req.screen_rect.loc.y as f64).into(),
                (win_size.w as f64, win_size.h as f64).into(),
            );
            let _ = frame.render_texture_from_to(
                &bg_src,
                src_rect,
                Rectangle::from_size(win_size),
                &[Rectangle::from_size(win_size)],
                &[],
                Transform::Normal,
                1.0,
                None,
                &[],
            );
            let _ = frame.finish();
        }

        // Run Kawase blur passes
        let offset = blur_strength * output_scale as f32;
        let _ = render_blur(
            renderer,
            &down_shader,
            &up_shader,
            &mut cache.texture,
            &mut cache.scratch,
            offset,
            blur_passes,
        );
    }

    // ── Loop 2: mask render + apply for all dirty windows (safe to overwrite bg_tex) ──
    for (i, req) in blur_requests.iter().enumerate() {
        if !needs_recompute[i] { continue; }
        let win_size = req.screen_rect.size;
        if win_size.w <= 0 || win_size.h <= 0 { continue; }

        let prefix = match req.layer {
            BlurLayer::Overlay => overlay_prefix,
            BlurLayer::Top => top_prefix,
            BlurLayer::Normal => normal_prefix,
            BlurLayer::Widget => widget_prefix,
        };

        // Render surface elements to bg_tex to capture alpha channel
        // index_shift is 0 here — element insertion hasn't happened yet
        let surf_start = prefix + req.elem_start;
        let surf_end = (surf_start + req.elem_count).min(all_elements.len());
        {
            let Ok(mut target) = renderer.bind(&mut bg_tex) else { continue };
            let mut dt = OutputDamageTracker::new(output_size, output_scale, Transform::Normal);
            let _ = dt.render_output(
                renderer,
                &mut target,
                0,
                &all_elements[surf_start..surf_end],
                [0.0f32, 0.0, 0.0, 0.0],
            );
        }

        let Some(cache) = state.render.blur_cache.get_mut(&req.surface_id) else { continue };

        // Crop surface region into cache.mask
        {
            let bg_src = bg_tex.clone();
            let Ok(mut target) = renderer.bind(&mut cache.mask) else { continue };
            let Ok(mut frame) = renderer.render(&mut target, win_size, Transform::Normal) else { continue };
            let _ = frame.clear(Color32F::TRANSPARENT, &[Rectangle::from_size(win_size)]);
            let src_rect: Rectangle<f64, smithay::utils::Buffer> = Rectangle::new(
                (req.screen_rect.loc.x as f64, req.screen_rect.loc.y as f64).into(),
                (win_size.w as f64, win_size.h as f64).into(),
            );
            let _ = frame.render_texture_from_to(
                &bg_src,
                src_rect,
                Rectangle::from_size(win_size),
                &[Rectangle::from_size(win_size)],
                &[],
                Transform::Normal,
                1.0,
                None,
                &[],
            );
            let _ = frame.finish();
        }

        // Masking pass — threshold surface alpha, multiply blur by it
        let Some(ref shader) = mask_shader else { continue };
        {
            use smithay::backend::renderer::gles::ffi;
            let mask_src = cache.mask.clone();
            let Ok(mut target) = renderer.bind(&mut cache.texture) else { continue };
            let Ok(mut frame) = renderer.render(&mut target, win_size, Transform::Normal) else { continue };
            let _ = frame.with_context(|gl| unsafe {
                gl.Enable(ffi::BLEND);
                gl.BlendFuncSeparate(
                    ffi::ZERO, ffi::SRC_ALPHA,
                    ffi::ZERO, ffi::SRC_ALPHA,
                );
            });
            let _ = frame.render_texture_from_to(
                &mask_src,
                Rectangle::from_size((win_size.w as f64, win_size.h as f64).into()),
                Rectangle::from_size(win_size),
                &[Rectangle::from_size(win_size)],
                &[],
                Transform::Normal,
                1.0,
                Some(shader),
                &[],
            );
            let _ = frame.with_context(|gl| unsafe {
                gl.BlendFunc(ffi::ONE, ffi::ONE_MINUS_SRC_ALPHA);
            });
            let _ = frame.finish();
        }

        cache.dirty = false;
    }

    // ── Insert blur elements for all windows (dirty or cached) ──
    let mut index_shift = 0usize;
    for req in blur_requests.iter() {
        let win_size = req.screen_rect.size;
        if win_size.w <= 0 || win_size.h <= 0 { continue; }
        let Some(cache) = state.render.blur_cache.get(&req.surface_id) else { continue };

        let prefix = match req.layer {
            BlurLayer::Overlay => overlay_prefix,
            BlurLayer::Top => top_prefix,
            BlurLayer::Normal => normal_prefix,
            BlurLayer::Widget => widget_prefix,
        };
        let insert_idx = prefix + req.elem_start + req.elem_count + index_shift;
        let insert_idx = insert_idx.min(all_elements.len());
        let blur_elem = TextureRenderElement::from_static_texture(
            Id::new(),
            context_id.clone(),
            req.screen_rect.loc.to_f64(),
            cache.texture.clone(),
            1,
            Transform::Normal,
            None,
            None,
            Some(Size::from((
                (win_size.w as f64 / output_scale) as i32,
                (win_size.h as f64 / output_scale) as i32,
            ))),
            None,
            Kind::Unspecified,
        );
        all_elements.insert(insert_idx, OutputRenderElements::Blur(blur_elem));
        index_shift += 1;
    }

    // Cache bg_tex back for next frame
    state.render.blur_bg_fbo = Some((bg_tex, output_size));
}

/// Which element group a blur request belongs to — determines its prefix offset.
#[derive(Clone, Copy)]
enum BlurLayer { Overlay, Top, Normal, Widget }

/// Data extracted from a blur request.
struct BlurRequestData {
    surface_id: smithay::reexports::wayland_server::backend::ObjectId,
    screen_rect: Rectangle<i32, Physical>,
    elem_start: usize,
    elem_count: usize,
    layer: BlurLayer,
}

/// Draw thin outlines showing where other monitors' viewports sit on the canvas.
fn build_output_outline_elements(
    state: &crate::state::DriftWm,
    renderer: &mut GlesRenderer,
    output: &Output,
    camera: Point<f64, Logical>,
    zoom: f64,
    viewport_size: Size<i32, Logical>,
) -> Vec<OutputRenderElements> {
    let thickness = state.config.output_outline.thickness;
    if thickness <= 0 { return vec![]; }

    let opacity = state.config.output_outline.opacity as f32;
    if opacity <= 0.0 { return vec![]; }
    let color = state.config.output_outline.color;
    let scale = output.current_scale().fractional_scale();

    let mut elements = Vec::new();

    for other in state.space.outputs() {
        if *other == *output { continue }

        let (other_camera, other_zoom) = {
            let os = crate::state::output_state(other);
            (os.camera, os.zoom)
        };
        let other_size = crate::state::output_logical_size(other);

        // Other output's visible canvas rect
        let other_canvas = canvas::visible_canvas_rect(
            other_camera.to_i32_round(),
            other_size,
            other_zoom,
        );

        // Transform to screen coords on *this* output
        let screen_x = ((other_canvas.loc.x as f64 - camera.x) * zoom) as i32;
        let screen_y = ((other_canvas.loc.y as f64 - camera.y) * zoom) as i32;
        let screen_w = (other_canvas.size.w as f64 * zoom) as i32;
        let screen_h = (other_canvas.size.h as f64 * zoom) as i32;

        // Clip to viewport
        let vp = Rectangle::from_size(viewport_size);
        let outline_rect = Rectangle::new((screen_x, screen_y).into(), (screen_w, screen_h).into());
        if !vp.overlaps(outline_rect) { continue }

        // Draw 4 edges as thin filled buffers
        let edges: [(i32, i32, i32, i32); 4] = [
            (screen_x, screen_y, screen_w, thickness),                         // top
            (screen_x, screen_y + screen_h - thickness, screen_w, thickness),  // bottom
            (screen_x, screen_y, thickness, screen_h),                         // left
            (screen_x + screen_w - thickness, screen_y, thickness, screen_h),  // right
        ];

        for (ex, ey, ew, eh) in edges {
            // Clip edge to viewport
            let x0 = ex.max(0);
            let y0 = ey.max(0);
            let x1 = (ex + ew).min(viewport_size.w);
            let y1 = (ey + eh).min(viewport_size.h);
            if x1 <= x0 || y1 <= y0 { continue }

            let w = x1 - x0;
            let h = y1 - y0;

            let pixels: Vec<u8> = vec![color[0], color[1], color[2], color[3]]
                .into_iter()
                .cycle()
                .take((w * h) as usize * 4)
                .collect();

            let buf = MemoryRenderBuffer::from_slice(
                &pixels,
                Fourcc::Abgr8888,
                (w, h),
                1,
                Transform::Normal,
                None,
            );

            let loc: Point<f64, Physical> = Point::from((x0, y0)).to_f64().to_physical(scale);
            if let Ok(elem) = MemoryRenderBufferRenderElement::from_buffer(
                renderer, loc, &buf, Some(opacity), None, None, Kind::Unspecified,
            ) {
                elements.push(OutputRenderElements::Decoration(
                    RescaleRenderElement::from_element(
                        elem,
                        Point::<i32, Physical>::from((0, 0)),
                        1.0,
                    ),
                ));
            }
        }
    }

    elements
}

/// Compile background shader and/or load tile image.
/// Called at startup and on config reload (lazy re-init).
/// On failure, falls back to `DEFAULT_SHADER` — never leaves background uninitialized.
pub fn init_background(state: &mut crate::state::DriftWm, renderer: &mut GlesRenderer, initial_size: Size<i32, smithay::utils::Logical>, output_name: &str) {
    // Try loading tile image first (if configured and no shader_path)
    if state.config.background.shader_path.is_none()
        && let Some(path) = state.config.background.tile_path.as_deref()
    {
        match image::open(path) {
            Ok(img) => {
                let img = img.into_rgba8();
                let (w, h) = img.dimensions();
                let raw = img.into_raw();

                use smithay::backend::renderer::ImportMem;
                use smithay::utils::Buffer;
                match renderer.import_memory(
                    &raw,
                    Fourcc::Abgr8888,
                    Size::<i32, Buffer>::from((w as i32, h as i32)),
                    false,
                ) {
                    Ok(texture) => {
                        if state.render.tile_shader.is_none() {
                            state.render.tile_shader = compile_tile_bg_shader(renderer);
                        }
                        if let Some(ref shader) = state.render.tile_shader {
                            let tw = w as i32;
                            let th = h as i32;
                            let area = Rectangle::from_size(initial_size);
                            let elem = TileShaderElement::new(
                                shader.clone(),
                                texture,
                                tw,
                                th,
                                area,
                                Some(vec![area]),
                                1.0,
                                vec![
                                    Uniform::new("u_camera", (0.0f32, 0.0f32)),
                                    Uniform::new("u_tile_size", (tw as f32, th as f32)),
                                    Uniform::new("u_output_size", (initial_size.w as f32, initial_size.h as f32)),
                                ],
                                Kind::Unspecified,
                            );
                            state.render.cached_tile_bg.insert(output_name.to_string(), elem);
                            return;
                        }
                        tracing::error!("Tile shader compilation failed, using default shader");
                    }
                    Err(e) => {
                        tracing::error!("Failed to upload tile texture: {e}, using default shader");
                    }
                }
            }
            Err(e) => {
                tracing::error!("Failed to load tile image {path}: {e}, using default shader");
            }
        }
    }

    // Reuse cached shader if already compiled (avoids redundant GPU work
    // when multiple outputs each need a background element).
    let shader = if let Some(ref cached) = state.render.background_shader {
        cached.clone()
    } else {
        let shader_source = if let Some(path) = state.config.background.shader_path.as_deref() {
            match std::fs::read_to_string(path) {
                Ok(src) => src,
                Err(e) => {
                    tracing::error!("Failed to read shader {path}: {e}, using default");
                    driftwm::config::DEFAULT_SHADER.to_string()
                }
            }
        } else {
            driftwm::config::DEFAULT_SHADER.to_string()
        };

        let compiled = match renderer.compile_custom_pixel_shader(&shader_source, BG_UNIFORMS) {
            Ok(s) => s,
            Err(e) => {
                tracing::error!("Failed to compile shader: {e}, using default");
                renderer
                    .compile_custom_pixel_shader(driftwm::config::DEFAULT_SHADER, BG_UNIFORMS)
                    .expect("Default shader must compile")
            }
        };
        state.render.background_shader = Some(compiled.clone());
        compiled
    };

    let area = Rectangle::from_size(initial_size);
    state.render.cached_bg_elements.insert(output_name.to_string(), PixelShaderElement::new(
        shader,
        area,
        Some(vec![area]),
        1.0,
        vec![Uniform::new("u_camera", (0.0f32, 0.0f32))],
        Kind::Unspecified,
    ));
}

/// Get or create persistent capture state for an output+protocol pair.
fn get_capture_state<'a>(
    map: &'a mut std::collections::HashMap<String, crate::state::CaptureOutputState>,
    key: &str,
    size: Size<i32, Physical>,
    scale: Scale<f64>,
    transform: Transform,
    paint_cursors: bool,
) -> &'a mut crate::state::CaptureOutputState {
    map.entry(key.to_owned()).or_insert_with(|| {
        crate::state::CaptureOutputState {
            damage_tracker: smithay::backend::renderer::damage::OutputDamageTracker::new(size, scale, transform),
            offscreen_texture: None,
            age: 0,
            last_paint_cursors: paint_cursors,
        }
    })
}

/// Fulfill pending screencopy requests by rendering to offscreen textures.
pub fn render_screencopy(
    state: &mut crate::state::DriftWm,
    renderer: &mut GlesRenderer,
    output: &Output,
    elements: &[OutputRenderElements],
) {
    use smithay::backend::renderer::{ExportMem, Renderer};
    use smithay::wayland::shm;
    use driftwm::protocols::screencopy::ScreencopyBuffer;
    use std::ptr;

    // Extract only requests for this output, keep the rest
    let mut pending = Vec::new();
    let mut i = 0;
    while i < state.pending_screencopies.len() {
        if state.pending_screencopies[i].output() == output {
            pending.push(state.pending_screencopies.swap_remove(i));
        } else {
            i += 1;
        }
    }

    if pending.is_empty() {
        return;
    }

    let output_scale = output.current_scale().fractional_scale();
    let scale = Scale::from(output_scale);
    let transform = output.current_transform();
    let output_mode_size = output.current_mode().unwrap().size;
    let timestamp = state.start_time.elapsed();
    let capture_key = format!("sc:{}", output.name());

    for screencopy in pending {
        let size = screencopy.buffer_size();
        let paint_cursors = screencopy.overlay_cursor();
        let use_elements: Vec<&OutputRenderElements> = if paint_cursors {
            elements.iter().collect()
        } else {
            elements
                .iter()
                .filter(|e| !matches!(e, OutputRenderElements::Cursor(_) | OutputRenderElements::CursorSurface(_)))
                .collect()
        };

        // Use persistent state for full-output captures (screen recording);
        // one-shot for region captures (partial screenshots).
        let use_persistent = size == output_mode_size;

        if use_persistent
            && let Some(cs) = state.render.capture_state.get_mut(&capture_key)
            && cs.last_paint_cursors != paint_cursors
        {
            cs.age = 0;
            cs.last_paint_cursors = paint_cursors;
        }

        match screencopy.buffer() {
            ScreencopyBuffer::Dmabuf(dmabuf) => {
                let mut dmabuf = dmabuf.clone();
                let cs = if use_persistent {
                    Some(get_capture_state(&mut state.render.capture_state, &capture_key, size, scale, transform, paint_cursors))
                } else {
                    None
                };
                match render_to_dmabuf(renderer, &mut dmabuf, size, scale, transform, &use_elements, cs) {
                    Ok(sync) => {
                        if let Err(e) = renderer.wait(&sync) {
                            tracing::warn!("screencopy: dmabuf sync wait failed: {e:?}");
                            continue; // screencopy Drop sends failed()
                        }
                        screencopy.submit(false, timestamp);
                    }
                    Err(e) => {
                        tracing::warn!("screencopy: dmabuf render failed: {e:?}");
                    }
                }
            }
            ScreencopyBuffer::Shm(wl_buffer) => {
                let cs = if use_persistent {
                    Some(get_capture_state(&mut state.render.capture_state, &capture_key, size, scale, transform, paint_cursors))
                } else {
                    None
                };
                let result = render_to_offscreen(renderer, size, scale, transform, &use_elements, cs);
                match result {
                    Ok(mapping) => {
                        let copy_ok =
                            shm::with_buffer_contents_mut(wl_buffer, |shm_buf, shm_len, _data| {
                                let bytes = match renderer.map_texture(&mapping) {
                                    Ok(b) => b,
                                    Err(e) => {
                                        tracing::warn!("screencopy: map_texture failed: {e:?}");
                                        return false;
                                    }
                                };
                                let copy_len = shm_len.min(bytes.len());
                                unsafe {
                                    ptr::copy_nonoverlapping(bytes.as_ptr(), shm_buf.cast(), copy_len);
                                }
                                true
                            });

                        match copy_ok {
                            Ok(true) => {
                                screencopy.submit(false, timestamp);
                            }
                            _ => {
                                tracing::warn!("screencopy: SHM buffer copy failed");
                            }
                        }
                    }
                    Err(e) => {
                        tracing::warn!("screencopy: offscreen render failed: {e:?}");
                    }
                }
            }
        }
    }
}

/// Render elements to an offscreen texture and download the pixels.
/// When `capture_state` is provided, reuses the damage tracker and texture across frames
/// for incremental rendering. Falls back to one-shot (age=0) when None.
fn render_to_offscreen(
    renderer: &mut GlesRenderer,
    size: smithay::utils::Size<i32, Physical>,
    scale: Scale<f64>,
    transform: Transform,
    elements: &[&OutputRenderElements],
    capture_state: Option<&mut crate::state::CaptureOutputState>,
) -> Result<smithay::backend::renderer::gles::GlesMapping, Box<dyn std::error::Error>> {
    use smithay::backend::renderer::{Bind, ExportMem, Offscreen};
    use smithay::backend::renderer::damage::OutputDamageTracker;
    use smithay::backend::renderer::gles::GlesTexture;

    let buffer_size = size.to_logical(1).to_buffer(1, Transform::Normal);

    if let Some(cs) = capture_state {
        // Reuse or reallocate texture when size changes
        let tex = match &mut cs.offscreen_texture {
            Some((tex, cached_size)) if *cached_size == size => tex,
            slot => {
                let new_tex: GlesTexture = Offscreen::<GlesTexture>::create_buffer(renderer, Fourcc::Xrgb8888, buffer_size)?;
                *slot = Some((new_tex, size));
                cs.damage_tracker = OutputDamageTracker::new(size, scale, transform);
                cs.age = 0;
                &mut slot.as_mut().unwrap().0
            }
        };

        {
            let mut target = renderer.bind(tex)?;
            let _ = cs.damage_tracker.render_output(
                renderer,
                &mut target,
                cs.age,
                elements,
                [0.0f32, 0.0, 0.0, 1.0],
            )?;
        }
        cs.age += 1;

        let target = renderer.bind(tex)?;
        let mapping = renderer.copy_framebuffer(&target, Rectangle::from_size(buffer_size), Fourcc::Xrgb8888)?;
        Ok(mapping)
    } else {
        let mut texture: GlesTexture = Offscreen::<GlesTexture>::create_buffer(renderer, Fourcc::Xrgb8888, buffer_size)?;
        {
            let mut target = renderer.bind(&mut texture)?;
            let mut damage_tracker = OutputDamageTracker::new(size, scale, transform);
            let _ = damage_tracker.render_output(
                renderer,
                &mut target,
                0,
                elements,
                [0.0f32, 0.0, 0.0, 1.0],
            )?;
        }
        let target = renderer.bind(&mut texture)?;
        let mapping = renderer.copy_framebuffer(&target, Rectangle::from_size(buffer_size), Fourcc::Xrgb8888)?;
        Ok(mapping)
    }
}

/// Render elements directly into a client-provided DMA-BUF (zero CPU copies).
///
/// The caller must choose the correct `transform` for the protocol:
/// - wlr-screencopy: `output.current_transform()` (buffer is raw mode size)
/// - ext-image-copy-capture: `Transform::Normal` (buffer is already transformed)
///
/// When `capture_state` is provided, reuses the damage tracker for incremental rendering.
fn render_to_dmabuf(
    renderer: &mut GlesRenderer,
    dmabuf: &mut smithay::backend::allocator::dmabuf::Dmabuf,
    size: Size<i32, Physical>,
    scale: Scale<f64>,
    transform: Transform,
    elements: &[&OutputRenderElements],
    capture_state: Option<&mut crate::state::CaptureOutputState>,
) -> Result<smithay::backend::renderer::sync::SyncPoint, Box<dyn std::error::Error>> {
    use smithay::backend::renderer::Bind;
    use smithay::backend::renderer::damage::OutputDamageTracker;

    let sync = match capture_state {
        Some(cs) => {
            let mut target = renderer.bind(dmabuf)?;
            let result = cs.damage_tracker.render_output(
                renderer,
                &mut target,
                cs.age,
                elements,
                [0.0f32, 0.0, 0.0, 1.0],
            )?.sync;
            cs.age += 1;
            result
        }
        None => {
            let mut target = renderer.bind(dmabuf)?;
            let mut damage_tracker = OutputDamageTracker::new(size, scale, transform);
            damage_tracker.render_output(
                renderer,
                &mut target,
                0,
                elements,
                [0.0f32, 0.0, 0.0, 1.0],
            )?.sync
        }
    };

    Ok(sync)
}

/// Fulfill pending ext-image-copy-capture frames by rendering to offscreen textures.
pub fn render_capture_frames(
    state: &mut crate::state::DriftWm,
    renderer: &mut GlesRenderer,
    output: &Output,
    elements: &[OutputRenderElements],
) {
    use smithay::backend::renderer::{ExportMem, Renderer};
    use smithay::wayland::shm;
    use std::ptr;

    // Promote any sessions waiting for damage on this output
    state
        .image_copy_capture_state
        .promote_waiting_frames(output, &mut state.pending_captures);

    // Extract captures for this output
    let mut pending = Vec::new();
    let mut i = 0;
    while i < state.pending_captures.len() {
        if &state.pending_captures[i].output == output {
            pending.push(state.pending_captures.swap_remove(i));
        } else {
            i += 1;
        }
    }

    if pending.is_empty() {
        return;
    }

    let output_scale = output.current_scale().fractional_scale();
    let scale = Scale::from(output_scale);
    let output_transform = output.current_transform();
    let output_mode_size = output_transform.transform_size(output.current_mode().unwrap().size);
    let timestamp = state.start_time.elapsed();
    let capture_key = format!("cap:{}", output.name());

    let fail_reason = smithay::reexports::wayland_protocols::ext::image_copy_capture::v1::server::ext_image_copy_capture_frame_v1::FailureReason::Unknown;

    for capture in pending {
        let paint_cursors = capture.paint_cursors;
        let use_elements: Vec<&OutputRenderElements> = if paint_cursors {
            elements.iter().collect()
        } else {
            elements
                .iter()
                .filter(|e| !matches!(e, OutputRenderElements::Cursor(_) | OutputRenderElements::CursorSurface(_)))
                .collect()
        };

        // ext-image-copy-capture buffer_size is already in transformed/logical orientation,
        // matching the element coordinate space — render with Normal (no additional transform).
        let use_persistent = capture.buffer_size == output_mode_size;

        if use_persistent
            && let Some(cs) = state.render.capture_state.get_mut(&capture_key)
            && cs.last_paint_cursors != paint_cursors
        {
            cs.age = 0;
            cs.last_paint_cursors = paint_cursors;
        }

        // Try DMA-BUF first, fall back to SHM
        let ok = if let Ok(dmabuf) = smithay::wayland::dmabuf::get_dmabuf(&capture.buffer) {
            let mut dmabuf = dmabuf.clone();
            let cs = if use_persistent {
                Some(get_capture_state(&mut state.render.capture_state, &capture_key, capture.buffer_size, scale, Transform::Normal, paint_cursors))
            } else {
                None
            };
            match render_to_dmabuf(renderer, &mut dmabuf, capture.buffer_size, scale, Transform::Normal, &use_elements, cs) {
                Ok(sync) => {
                    if let Err(e) = renderer.wait(&sync) {
                        tracing::warn!("capture: dmabuf sync wait failed: {e:?}");
                        false
                    } else {
                        true
                    }
                }
                Err(e) => {
                    tracing::warn!("capture: dmabuf render failed: {e:?}");
                    false
                }
            }
        } else {
            let cs = if use_persistent {
                Some(get_capture_state(&mut state.render.capture_state, &capture_key, capture.buffer_size, scale, Transform::Normal, paint_cursors))
            } else {
                None
            };
            let result = render_to_offscreen(renderer, capture.buffer_size, scale, Transform::Normal, &use_elements, cs);
            match result {
                Ok(mapping) => {
                    shm::with_buffer_contents_mut(&capture.buffer, |shm_buf, shm_len, _data| {
                        let bytes = match renderer.map_texture(&mapping) {
                            Ok(b) => b,
                            Err(e) => {
                                tracing::warn!("capture: map_texture failed: {e:?}");
                                return false;
                            }
                        };
                        let copy_len = shm_len.min(bytes.len());
                        unsafe {
                            ptr::copy_nonoverlapping(bytes.as_ptr(), shm_buf.cast(), copy_len);
                        }
                        true
                    })
                    .unwrap_or(false)
                }
                Err(e) => {
                    tracing::warn!("capture: offscreen render failed: {e:?}");
                    false
                }
            }
        };

        if ok {
            let w = capture.buffer_size.w;
            let h = capture.buffer_size.h;
            capture.frame.transform(smithay::utils::Transform::Normal.into());
            capture.frame.damage(0, 0, w, h);
            let tv_sec_hi = (timestamp.as_secs() >> 32) as u32;
            let tv_sec_lo = (timestamp.as_secs() & 0xFFFFFFFF) as u32;
            let tv_nsec = timestamp.subsec_nanos();
            capture.frame.presentation_time(tv_sec_hi, tv_sec_lo, tv_nsec);
            capture.frame.ready();

            let frame_data = capture.frame.data::<std::sync::Mutex<driftwm::protocols::image_copy_capture::CaptureFrameData>>();
            if let Some(fd) = frame_data {
                let fd = fd.lock().unwrap();
                state.image_copy_capture_state.frame_done(&fd.session);
            }
        } else {
            capture.frame.failed(fail_reason);
        }
    }
}

/// Sync foreign-toplevel protocol state with the current window list.
/// Call once per frame iteration (not per-output).
pub fn refresh_foreign_toplevels(state: &mut crate::state::DriftWm) {
    let keyboard = state.seat.get_keyboard().unwrap();
    let focused = keyboard.current_focus().map(|f| f.0);
    let outputs: Vec<Output> = state.space.outputs().cloned().collect();
    driftwm::protocols::foreign_toplevel::refresh::<crate::state::DriftWm>(
        &mut state.foreign_toplevel_state,
        &state.space,
        focused.as_ref(),
        &outputs,
    );
}

/// Post-render: frame callbacks, space cleanup.
pub fn post_render(state: &mut crate::state::DriftWm, output: &Output) {
    // Frame callbacks to windows
    let time = state.start_time.elapsed();
    for window in state.space.elements() {
        window.send_frame(output, time, Some(Duration::ZERO), |_, _| {
            Some(output.clone())
        });
    }

    // Layer surface frame callbacks
    {
        let layer_map = layer_map_for_output(output);
        for layer_surface in layer_map.layers() {
            layer_surface.send_frame(output, time, Some(Duration::ZERO), |_, _| {
                Some(output.clone())
            });
        }
    }

    // Canvas-positioned layer surface frame callbacks
    for cl in &state.canvas_layers {
        cl.surface.send_frame(output, time, Some(Duration::ZERO), |_, _| {
            Some(output.clone())
        });
    }

    // Override-redirect X11 surface frame callbacks
    for or_surface in &state.x11_override_redirect {
        if let Some(wl_surface) = or_surface.wl_surface() {
            smithay::desktop::utils::send_frames_surface_tree(
                &wl_surface, output, time, Some(Duration::ZERO),
                |_, _| Some(output.clone()),
            );
        }
    }

    // Cursor surface frame callbacks (animated cursors need these to advance)
    if let CursorImageStatus::Surface(ref surface) = state.cursor.cursor_status {
        smithay::desktop::utils::send_frames_surface_tree(
            surface, output, time, Some(Duration::ZERO),
            |_, _| Some(output.clone()),
        );
    }

    // Lock surface frame callback
    if let Some(lock_surface) = state.lock_surfaces.get(output) {
        smithay::desktop::utils::send_frames_surface_tree(
            lock_surface.wl_surface(),
            output,
            time,
            Some(Duration::ZERO),
            |_, _| Some(output.clone()),
        );
    }

    // Cleanup
    state.space.refresh();
    state.popups.cleanup();
    layer_map_for_output(output).cleanup();
}
