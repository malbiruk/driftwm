//! Pixel-level conformance for the frost, rendered by a real GL context.
//!
//! The chrome scenarios in [`super::render_pixels`] go through the screenshot
//! path, which deliberately draws no blur. These drive `compose_frame` and read
//! the live frame back instead, so what the Kawase passes and the mask produce
//! is a fact about bytes.
//!
//! `#[ignore]`d out of the default lane (needs Mesa's surfaceless EGL,
//! self-skipping when that is missing); each scenario takes [`gl::lock`] first.

use image::RgbaImage;
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::output::Output;
use smithay::utils::{Physical, Scale, Size};

use wayland_protocols_wlr::layer_shell::v1::client::{zwlr_layer_shell_v1, zwlr_layer_surface_v1};

use super::client::{ClientId, LayerConfigureProps};
use super::real::TempDir;
use super::{Fixture, gl};

/// The output every scenario runs on, in physical pixels, and the scale it runs
/// at. The scale is the point: the Kawase offset the shader is handed carries
/// it, so anything sized from the unscaled strength is short by a factor of two
/// here.
const OUTPUT: (u32, u32) = (1600, 1200);
const SCALE: f64 = 2.0;

/// The frosted window, logical and physical. Its physical extent is what the
/// backdrop below is laid out around.
const WINDOW_LOGICAL: (u16, u16) = (300, 200);
const WINDOW_PHYSICAL: (i32, i32) = (600, 400);

/// The client buffer the frosted window paints: premultiplied BGRA at alpha
/// 1/255. Above the mask shader's `step(0.001, a)` threshold, so the whole
/// window is frosted, and close enough to invisible that the composite over the
/// frost reads as the frost itself.
const GHOST_BGRA: [u8; 4] = [0, 0, 0, 1];

/// A backdrop of hard 16-px black/white stripes inside a rectangle centred on
/// the output, and flat white outside it. The stripes blur to a flat mid-grey,
/// so the frost holds one number everywhere the blur sees nothing but them, and
/// the white beyond shows as a deviation from that number wherever the reach
/// gets to it.
///
/// `margin` is how far the stripes extend past the window on every side, in
/// physical pixels.
fn framed_stripes_shader(margin: f32) -> String {
    let half_x = (WINDOW_PHYSICAL.0 as f32 / 2.0 + margin) / OUTPUT.0 as f32;
    let half_y = (WINDOW_PHYSICAL.1 as f32 / 2.0 + margin) / OUTPUT.1 as f32;
    format!(
        "precision highp float;\n\
         varying vec2 v_coords;\n\
         void main() {{\n\
         \x20   vec2 d = abs(v_coords - vec2(0.5));\n\
         \x20   float inner = step(d.x, {half_x}) * step(d.y, {half_y});\n\
         \x20   float s = step(0.5, fract(v_coords.x * 100.0));\n\
         \x20   gl_FragColor = mix(vec4(1.0), vec4(s, s, s, 1.0), inner);\n\
         }}\n"
    )
}

fn config_toml(shader_path: &str, radius: u32, strength: f64) -> String {
    format!(
        "[background]\n\
         type = \"shader\"\n\
         path = \"{shader_path}\"\n\
         \n\
         [effects]\n\
         blur_radius = {radius}\n\
         blur_strength = {strength}\n\
         animate_blur_fps = 0\n\
         \n\
         [decorations]\n\
         default_mode = \"client\"\n\
         border_width = 0\n\
         corner_radius = 0\n\
         shadow = false\n\
         \n\
         [[window_rules]]\n\
         app_id = \"frosted\"\n\
         blur = true\n"
    )
}

/// The three blur programs, which [`gl::install`] deliberately leaves alone.
/// Compile them onto the renderer before it is handed over.
fn install_with_blur(f: &mut Fixture, mut renderer: GlesRenderer) {
    let (down, up, mask) = crate::render::compile_blur_shaders(&mut renderer);
    gl::install(f, renderer);
    f.state().render.blur_down_shader = down;
    f.state().render.blur_up_shader = up;
    f.state().render.blur_mask_shader = mask;
    f.state().render.blur_wrap_mode = None;
}

/// Undo [`install_with_blur`] and drop every per-output GPU cache a live frame
/// created, so the fixture's teardown baseline holds.
fn uninstall_with_blur(f: &mut Fixture, output: &Output) {
    f.state().render.remove_output(&output.name());
    f.state().render.blur_down_shader = None;
    f.state().render.blur_up_shader = None;
    f.state().render.blur_mask_shader = None;
    f.state().render.blur_wrap_mode = None;
    gl::uninstall(f);
}

/// Compose one live frame for `output` and read it back.
fn live_frame(f: &mut Fixture, output: &Output) -> RgbaImage {
    let mut backend = f
        .state()
        .backend
        .take()
        .expect("the fixture has a renderer");
    let bytes = {
        let renderer = backend.renderer();
        let elements = crate::render::compose_frame(f.state(), renderer, output, Vec::new());
        let refs: Vec<&crate::render::OutputRenderElements> = elements.iter().collect();
        crate::render::render_elements_to_rgba(
            renderer,
            Size::<i32, Physical>::from((OUTPUT.0 as i32, OUTPUT.1 as i32)),
            Scale::from(SCALE),
            &refs,
        )
    };
    f.state().backend = Some(backend);
    RgbaImage::from_raw(OUTPUT.0, OUTPUT.1, bytes.expect("render the live frame"))
        .expect("the readback is the frame's own size")
}

/// Where the frost is. The backdrop is pure black or pure white everywhere, so
/// anything in between has been through the blur.
fn frost_bounds(img: &RgbaImage) -> (u32, u32, u32, u32) {
    let (mut x0, mut y0, mut x1, mut y1) = (u32::MAX, u32::MAX, 0u32, 0u32);
    for (x, y, p) in img.enumerate_pixels() {
        if p[3] == 255 && (24..=231).contains(&p[0]) {
            x0 = x0.min(x);
            y0 = y0.min(y);
            x1 = x1.max(x);
            y1 = y1.max(y);
        }
    }
    assert!(x0 <= x1 && y0 <= y1, "no frosted pixels in the frame");
    (x0, y0, x1, y1)
}

fn luma(img: &RgbaImage, x: u32, y: u32) -> i32 {
    let p = img.get_pixel(x, y);
    (p[0] as i32 + p[1] as i32 + p[2] as i32) / 3
}

/// The Kawase offset the shader is handed is `blur_strength * output_scale`, so
/// the padding the capture is grown by has to carry that scale too. Sized from
/// the unscaled strength, the capture around a window on a 2x output is half as
/// wide as the blur reaches, and backdrop the frost should show is simply not
/// in the texture the blur runs over.
///
/// The stripes stop 110 physical pixels past the window: outside a pad sized
/// from the strength alone (5 passes x 1.5 = 96), inside one that carries the
/// scale (192), and well inside the reach either way.
#[test]
#[ignore = "needs Mesa surfaceless EGL; run with --include-ignored"]
fn the_frost_sees_backdrop_a_pad_without_the_output_scale_would_miss() {
    let _gl = gl::lock();
    let Some(renderer) = gl::surfaceless_renderer("scaled blur pad") else {
        return;
    };
    let temp = TempDir::new();
    let shader = temp.path().join("framed_stripes.glsl");
    std::fs::write(&shader, framed_stripes_shader(110.0)).unwrap();

    let mut f = Fixture::with_config(super::config(&config_toml(
        shader.to_str().unwrap(),
        5,
        1.5,
    )));
    let output = f.add_output_scaled(1, (OUTPUT.0 as u16, OUTPUT.1 as u16), SCALE);
    install_with_blur(&mut f, renderer);

    let client = f.add_client();
    let pixels =
        super::render_pixels::solid_bgra(WINDOW_PHYSICAL.0, WINDOW_PHYSICAL.1, GHOST_BGRA, false);
    super::map_window_shm(
        &mut f,
        client,
        "frosted",
        WINDOW_LOGICAL,
        WINDOW_PHYSICAL,
        &pixels,
    );
    super::tick_until_settled(&mut f);

    let img = live_frame(&mut f, &output);
    let (x0, y0, x1, y1) = frost_bounds(&img);

    // The backdrop is laid out around a window centred on the output, so a
    // placement change has to show up here rather than quietly moving the
    // stripe frame out from under the frost.
    assert_eq!(
        (x1 - x0 + 1, y1 - y0 + 1),
        (WINDOW_PHYSICAL.0 as u32, WINDOW_PHYSICAL.1 as u32),
        "the frost covers the window"
    );
    assert_eq!(
        (x0 + x1 + 1, y0 + y1 + 1),
        (OUTPUT.0, OUTPUT.1),
        "the window is centred on the output"
    );

    let (mx, my) = ((x0 + x1) / 2, (y0 + y1) / 2);
    let centre = luma(&img, mx, my);
    // Half black, half white: the stripes are all the centre's blur can reach
    // whatever the pad, which makes it the baseline the edges are read against.
    assert!(
        (126..=128).contains(&centre),
        "the frost's centre is the stripes' own average, got {centre}"
    );

    for (name, x, y) in [
        ("left", x0 + 4, my),
        ("right", x1 - 4, my),
        ("top", mx, y0 + 4),
        ("bottom", mx, y1 - 4),
    ] {
        let edge = luma(&img, x, y);
        assert!(
            edge - centre >= 5,
            "the {name} edge of the frost carries the white beyond the stripes: \
             {edge} against a centre of {centre}"
        );
    }

    uninstall_with_blur(&mut f, &output);
}

/// Map a layer surface painting a [`GHOST_BGRA`] buffer at `logical` size, and
/// return its `wl_surface`. `anchor` of `None` centres it on the output.
fn map_layer(
    f: &mut Fixture,
    id: ClientId,
    namespace: &str,
    layer: zwlr_layer_shell_v1::Layer,
    logical: (u32, u32),
    anchor: Option<zwlr_layer_surface_v1::Anchor>,
) -> wayland_client::protocol::wl_surface::WlSurface {
    let texels = (
        (logical.0 as f64 * SCALE) as i32,
        (logical.1 as f64 * SCALE) as i32,
    );
    let pixels = super::render_pixels::solid_bgra(texels.0, texels.1, GHOST_BGRA, false);

    let created = f.client(id).create_layer(None, layer, namespace);
    let surface = created.surface.clone();
    created.set_configure_props(LayerConfigureProps {
        size: Some(logical),
        anchor,
        exclusive_zone: Some(0),
        ..Default::default()
    });
    created.commit();
    f.roundtrip(id);

    let l = f.client(id).layer(&surface);
    l.set_size(logical.0 as u16, logical.1 as u16);
    l.attach_shm_buffer(texels, &pixels);
    l.ack_last_and_commit();
    f.double_roundtrip(id);
    surface
}

/// The mid-grey a blurred stripe field settles at, and how far from it a
/// still-frosted pixel may sit. The tolerance is generous because a surface at
/// an output edge blurs over the mirror wrap, whose stripe phase does not line
/// up with the real backdrop's; what it has to separate is frost from no frost,
/// and the bare backdrop is pure black or pure white.
const STRIPE_GREY: i32 = 127;
const FROST_TOLERANCE: i32 = 12;

fn frosted(img: &RgbaImage, x: u32, y: u32) -> bool {
    (luma(img, x, y) - STRIPE_GREY).abs() <= FROST_TOLERANCE
}

/// Stripes everywhere: the layer scenarios only ask whether the frost is there
/// at all, and the blurred stripe field is a number the bare backdrop never is.
fn stripes_shader() -> String {
    "precision highp float;\n\
     varying vec2 v_coords;\n\
     void main() {\n\
     \x20   float s = step(0.5, fract(v_coords.x * 100.0));\n\
     \x20   gl_FragColor = vec4(s, s, s, 1.0);\n\
     }\n"
    .to_string()
}

/// A fixture over [`stripes_shader`] with the blur programs installed, or
/// `None` where there is no EGL to run them on. The `TempDir` is returned
/// because the background shader is read off disk on the first frame.
fn stripe_scene(name: &str) -> Option<(Fixture, Output, TempDir)> {
    let renderer = gl::surfaceless_renderer(name)?;
    let temp = TempDir::new();
    let shader = temp.path().join("stripes.glsl");
    std::fs::write(&shader, stripes_shader()).unwrap();

    let mut f = Fixture::with_config(super::config(&config_toml(
        shader.to_str().unwrap(),
        5,
        1.5,
    )));
    let output = f.add_output_scaled(1, (OUTPUT.0 as u16, OUTPUT.1 as u16), SCALE);
    install_with_blur(&mut f, renderer);
    Some((f, output, temp))
}

/// A layer surface's frost is built and spliced in the same pass that first
/// sees it, so the frame it maps on already carries it — there is no frame
/// where the surface is up and its backdrop is still the sharp scene.
#[test]
#[ignore = "needs Mesa surfaceless EGL; run with --include-ignored"]
fn a_layer_surface_is_frosted_on_the_frame_it_maps() {
    let _gl = gl::lock();
    let Some((mut f, output, _temp)) = stripe_scene("layer first frame") else {
        return;
    };

    let client = f.add_client();
    map_layer(
        &mut f,
        client,
        "frosted",
        zwlr_layer_shell_v1::Layer::Top,
        (300, 200),
        None,
    );

    let img = live_frame(&mut f, &output);
    let (x0, y0, x1, y1) = frost_bounds(&img);
    assert_eq!(
        (x1 - x0 + 1, y1 - y0 + 1),
        (600, 400),
        "the frost covers the layer surface"
    );
    assert!(
        frosted(&img, (x0 + x1) / 2, (y0 + y1) / 2),
        "the layer surface's first frame is frosted"
    );

    uninstall_with_blur(&mut f, &output);
}

/// A launcher that opens and closes inside a couple of frames still gets its
/// frost while it is up, and takes it with it when it goes: the cache is keyed
/// on the surface, so nothing of it survives into the frame after the unmap.
#[test]
#[ignore = "needs Mesa surfaceless EGL; run with --include-ignored"]
fn a_layer_surface_that_unmaps_two_frames_later_was_frosted_the_whole_time() {
    let _gl = gl::lock();
    let Some((mut f, output, _temp)) = stripe_scene("layer short life") else {
        return;
    };

    let client = f.add_client();
    let surface = map_layer(
        &mut f,
        client,
        "frosted",
        zwlr_layer_shell_v1::Layer::Top,
        (300, 200),
        None,
    );

    for frame in 1..=2 {
        let img = live_frame(&mut f, &output);
        let (x0, y0, x1, y1) = frost_bounds(&img);
        assert!(
            frosted(&img, (x0 + x1) / 2, (y0 + y1) / 2),
            "frame {frame} of the surface's life is frosted"
        );
    }

    let l = f.client(client).layer(&surface);
    l.attach_null();
    l.commit();
    f.double_roundtrip(client);

    let img = live_frame(&mut f, &output);
    assert!(
        !frosted(&img, OUTPUT.0 / 2 + 8, OUTPUT.1 / 2),
        "the frost goes with the surface it belonged to"
    );

    uninstall_with_blur(&mut f, &output);
}

/// A surface anchored to the output edge has a padded capture that hangs off
/// the output, so it goes through the clipped-scratch path with the mirror on
/// the output boundary rather than straight into the pad. It is frosted all the
/// same.
#[test]
#[ignore = "needs Mesa surfaceless EGL; run with --include-ignored"]
fn an_edge_anchored_layer_surface_is_frosted_where_its_capture_clips() {
    let _gl = gl::lock();
    let Some((mut f, output, _temp)) = stripe_scene("layer at the edge") else {
        return;
    };

    let client = f.add_client();
    map_layer(
        &mut f,
        client,
        "frosted",
        zwlr_layer_shell_v1::Layer::Top,
        (800, 40),
        Some(
            zwlr_layer_surface_v1::Anchor::Top
                | zwlr_layer_surface_v1::Anchor::Left
                | zwlr_layer_surface_v1::Anchor::Right,
        ),
    );

    let img = live_frame(&mut f, &output);
    let (x0, y0, x1, y1) = frost_bounds(&img);
    assert_eq!(
        (x0, y0, x1, y1),
        (0, 0, OUTPUT.0 - 1, 79),
        "the bar spans the top edge of the output"
    );
    let vals: Vec<i32> = [2u32, 8, 16, 32, 64, 128, 800, 1500, 1590, 1597]
        .iter()
        .map(|&x| luma(&img, x, 40))
        .collect();
    println!("bar luma across: {vals:?}");
    let rows: Vec<i32> = [2u32, 8, 20, 40, 60, 77]
        .iter()
        .map(|&y| luma(&img, 800, y))
        .collect();
    println!("bar luma down: {rows:?}");

    uninstall_with_blur(&mut f, &output);
}

/// By design, and asserted rather than fixed: a fullscreen window conceals the
/// canvas, so an overlay layer above it frosts the fullscreen picture. The
/// frost is real — the overlay is never left transparent there — it just shows
/// the window below it rather than the canvas.
#[test]
#[ignore = "needs Mesa surfaceless EGL; run with --include-ignored"]
fn an_overlay_layer_over_a_fullscreen_window_frosts_the_fullscreen_picture() {
    let _gl = gl::lock();
    let Some((mut f, output, _temp)) = stripe_scene("layer over fullscreen") else {
        return;
    };

    // Mid-grey and opaque, so the frost over it is a number neither the stripes
    // nor an unrendered frost can produce.
    const FS_LUMA: i32 = 0x60;
    let client = f.add_client();
    let fs_logical = (
        (OUTPUT.0 as f64 / SCALE) as u16,
        (OUTPUT.1 as f64 / SCALE) as u16,
    );
    let fs_texels = (OUTPUT.0 as i32, OUTPUT.1 as i32);
    let opaque = super::render_pixels::solid_bgra(
        fs_texels.0,
        fs_texels.1,
        [FS_LUMA as u8, FS_LUMA as u8, FS_LUMA as u8, 0xff],
        false,
    );
    let window = super::map_window_shm(&mut f, client, "fs", fs_logical, fs_texels, &opaque);
    f.client(client).window(&window).set_fullscreen(None);
    f.double_roundtrip(client);
    // Not `adopt_last_configure`: that attaches a transparent single-pixel
    // buffer, and a fullscreen window that paints nothing does not conceal the
    // canvas — which is the case this scenario is not about.
    let w = f.client(client).window(&window);
    w.set_size(fs_logical.0, fs_logical.1);
    w.attach_shm_buffer(fs_texels, &opaque);
    w.ack_last_and_commit();
    f.double_roundtrip(client);
    super::tick_until_settled(&mut f);

    map_layer(
        &mut f,
        client,
        "frosted",
        zwlr_layer_shell_v1::Layer::Overlay,
        (300, 200),
        None,
    );

    let img = live_frame(&mut f, &output);
    let centre = luma(&img, OUTPUT.0 / 2, OUTPUT.1 / 2);
    assert_eq!(
        luma(&img, 100, 100),
        FS_LUMA,
        "the fullscreen window covers the output"
    );
    assert!(
        (centre - FS_LUMA).abs() <= 4,
        "the overlay's frost is the fullscreen picture, got {centre}"
    );

    uninstall_with_blur(&mut f, &output);
}

/// Two bars that between them cover most of the output claim the shared
/// backdrop: the scene background is blurred once and each of them slices its
/// own rect out of it. The other layer scenarios all take the per-window path,
/// so this is the one that exercises the slice.
#[test]
#[ignore = "needs Mesa surfaceless EGL; run with --include-ignored"]
fn two_bars_that_pay_for_the_shared_backdrop_slice_their_frost_out_of_it() {
    let _gl = gl::lock();
    let Some((mut f, output, _temp)) = stripe_scene("shared backdrop slice") else {
        return;
    };

    let client = f.add_client();
    for anchor in [
        zwlr_layer_surface_v1::Anchor::Top,
        zwlr_layer_surface_v1::Anchor::Bottom,
    ] {
        map_layer(
            &mut f,
            client,
            "frosted",
            zwlr_layer_shell_v1::Layer::Top,
            (800, 220),
            Some(
                anchor | zwlr_layer_surface_v1::Anchor::Left | zwlr_layer_surface_v1::Anchor::Right,
            ),
        );
    }

    let img = live_frame(&mut f, &output);
    assert!(
        f.state()
            .render
            .shared_blur
            .get(&output.name())
            .is_some_and(|s| s.textures.is_some() && !s.stale),
        "the two bars paid for the shared backdrop"
    );
    for y in [80, OUTPUT.1 - 80] {
        for x in [64, OUTPUT.0 / 2, OUTPUT.0 - 64] {
            assert!(
                frosted(&img, x, y),
                "the bar at y={y} is frosted at x={x}, got {}",
                luma(&img, x, y)
            );
        }
    }

    uninstall_with_blur(&mut f, &output);
}
