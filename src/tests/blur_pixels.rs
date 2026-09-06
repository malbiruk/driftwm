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
