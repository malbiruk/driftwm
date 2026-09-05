//! Pixel-level conformance for window chrome, rendered by a real GL context.
//!
//! The fixture gets a surfaceless GLES renderer (see [`super::gl`]), a client
//! maps a window with a real `wl_shm` texture, and the compositor's own capture
//! path writes a PNG the test reads back. Everything the shaders decide —
//! where the ring lands, how wide it is, whether its corners are square, what
//! the shadow masks — is then a fact about bytes rather than about geometry
//! arithmetic.
//!
//! `#[ignore]`d out of the default lane (needs Mesa's surfaceless EGL,
//! self-skipping when that is missing); each scenario takes [`gl::lock`] first.

use std::path::Path;

use image::{Rgba, RgbaImage};
use smithay::utils::{Physical, Scale, Size};

use super::real::{TempDir, ipc_request};
use super::{Fixture, gl, map_window_shm};
use crate::ipc::protocol::{Request, Response, ScreenshotTarget, WindowSelector};

/// The ring colour every scenario configures, focused and unfocused alike — the
/// focused default is independent, and the mapped window is focused.
const RING_RGB: [u8; 3] = [0x57, 0x52, 0x79];
/// Opaque red, as the client paints it: premultiplied little-endian BGRA.
const CONTENT_BGRA: [u8; 4] = [0, 0, 255, 255];
const CONTENT_RGB: [u8; 3] = [0xFF, 0x00, 0x00];

fn config_toml(shadow: bool) -> String {
    format!(
        "[background]\n\
         type = \"none\"\n\
         \n\
         [decorations]\n\
         default_mode = \"client\"\n\
         border_width = 2\n\
         border_color = \"#575279\"\n\
         border_color_focused = \"#575279\"\n\
         corner_radius = 0\n\
         shadow = {shadow}\n"
    )
}

/// The odd-sized window both the capture and the live-frame scenario use:
/// 1693x1053 logical lands on a half physical pixel at 1.5x, so the buffer the
/// fractional-scale protocol has the client allocate is a texel wider and
/// taller than the window paints. `ODD_*` are what its chrome measures there.
const ODD_LOGICAL: (u16, u16) = (1693, 1053);
const ODD_TEXELS: (i32, i32) = (2540, 1580);
const ODD_RING_PX: usize = 3;
const ODD_CONTENT_W: usize = 2539;
const ODD_CONTENT_H: usize = 1579;

/// Map [`ODD_LOGICAL`] with an [`ODD_TEXELS`] texture, its last column and row
/// left blank per [`solid_bgra`]'s `blank_last_col_row`.
fn map_odd_window(f: &mut Fixture, client: super::client::ClientId) {
    let pixels = solid_bgra(ODD_TEXELS.0, ODD_TEXELS.1, CONTENT_BGRA, true);
    map_window_shm(f, client, "pixels", ODD_LOGICAL, ODD_TEXELS, &pixels);
}

/// A premultiplied little-endian BGRA buffer of `w`x`h` texels filled with
/// `color`. `blank_last_col_row` leaves the far column and row transparent —
/// what a client paints into the extra texel `wp_fractional_scale_v1` rounds
/// its buffer up to when the logical size lands on a half physical pixel.
fn solid_bgra(w: i32, h: i32, color: [u8; 4], blank_last_col_row: bool) -> Vec<u8> {
    let mut buf = vec![0u8; (w * h * 4) as usize];
    for y in 0..h {
        for x in 0..w {
            if blank_last_col_row && (x == w - 1 || y == h - 1) {
                continue;
            }
            let i = ((y * w + x) * 4) as usize;
            buf[i..i + 4].copy_from_slice(&color);
        }
    }
    buf
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Px {
    Clear,
    /// Black at any alpha: the drop shadow, which no other element paints.
    Shadow,
    Ring,
    Content,
    Other,
}

/// Whether `p` is `rgb` premultiplied by its own alpha — true of a fully
/// covered pixel and of a partially covered one alike, so the classification
/// survives anti-aliasing and the alpha is left to be asserted separately.
fn premultiplied(p: Rgba<u8>, rgb: [u8; 3]) -> bool {
    let a = f32::from(p[3]);
    (0..3).all(|i| (f32::from(p[i]) - a * f32::from(rgb[i]) / 255.0).abs() <= 2.0)
}

fn classify(p: Rgba<u8>) -> Px {
    if p[3] == 0 {
        Px::Clear
    } else if p[0] == 0 && p[1] == 0 && p[2] == 0 {
        Px::Shadow
    } else if premultiplied(p, RING_RGB) {
        Px::Ring
    } else if premultiplied(p, CONTENT_RGB) {
        Px::Content
    } else {
        Px::Other
    }
}

/// Inclusive `(start, end)` index pairs of every maximal run of `want`.
fn runs_of(line: &[Px], want: Px) -> Vec<(usize, usize)> {
    let mut runs = Vec::new();
    let mut start = None;
    for (i, px) in line.iter().enumerate() {
        match (*px == want, start) {
            (true, None) => start = Some(i),
            (false, Some(s)) => {
                runs.push((s, i - 1));
                start = None;
            }
            _ => {}
        }
    }
    if let Some(s) = start {
        runs.push((s, line.len() - 1));
    }
    runs
}

fn row(img: &RgbaImage, y: u32) -> Vec<Rgba<u8>> {
    (0..img.width()).map(|x| *img.get_pixel(x, y)).collect()
}

fn column(img: &RgbaImage, x: u32) -> Vec<Rgba<u8>> {
    (0..img.height()).map(|y| *img.get_pixel(x, y)).collect()
}

/// What a line through the middle of a bordered window owes: a ring run of
/// `ring_px` on each side, `content_px` of client content between them with
/// nothing in between on either side, and no half-covered ring pixel — the
/// stroke's edges are integral, so its anti-aliasing band never widens past
/// the pixel it belongs to.
fn assert_ring_hugs_content(line: &[Rgba<u8>], ring_px: usize, content_px: usize, axis: &str) {
    let classes: Vec<Px> = line.iter().map(|p| classify(*p)).collect();

    if let Some(i) = classes
        .iter()
        .position(|c| !matches!(c, Px::Clear | Px::Ring | Px::Content))
    {
        panic!(
            "{axis}: pixel {i} is neither clear, ring, nor content: {:?}",
            line[i]
        );
    }

    let rings = runs_of(&classes, Px::Ring);
    assert_eq!(rings.len(), 2, "{axis}: ring runs are {rings:?}");
    for (start, end) in &rings {
        assert_eq!(end - start + 1, ring_px, "{axis}: ring run {start}..={end}");
        for (i, px) in line.iter().enumerate().take(*end + 1).skip(*start) {
            assert_eq!(
                px[3], 255,
                "{axis}: ring pixel {i} is only partly covered: {px:?}"
            );
        }
    }

    let content = runs_of(&classes, Px::Content);
    assert_eq!(content.len(), 1, "{axis}: content runs are {content:?}");
    let (start, end) = content[0];
    assert_eq!(end - start + 1, content_px, "{axis}: content run");
    assert_eq!(
        start,
        rings[0].1 + 1,
        "{axis}: background shows between the ring and the content"
    );
    assert_eq!(
        rings[1].0,
        end + 1,
        "{axis}: background shows between the content and the ring"
    );
}

/// The id of the fixture's only window, over the same IPC the capture uses.
fn only_window_id(f: &mut Fixture, ipc: &Path) -> u64 {
    let reply = ipc_request(f, ipc, &Request::State);
    let Ok(Response::State(info)) = reply else {
        panic!("expected a State reply, got {reply:?}");
    };
    assert_eq!(info.windows.len(), 1, "one window is mapped");
    info.windows[0].id
}

/// Capture window `id` in isolation at `scale` and read the PNG back.
fn capture_window(
    f: &mut Fixture,
    ipc: &Path,
    dir: &Path,
    name: &str,
    id: u64,
    scale: f64,
) -> RgbaImage {
    let path = dir.join(name);
    let reply = ipc_request(
        f,
        ipc,
        &Request::Screenshot {
            target: ScreenshotTarget::Window {
                window: Some(WindowSelector::Id(id)),
            },
            scale,
            path: path.to_string_lossy().into_owned(),
        },
    );
    let Ok(Response::Screenshot { width, height, .. }) = reply else {
        panic!("expected a Screenshot reply, got {reply:?}");
    };
    let img = image::open(&path)
        .expect("open the written capture")
        .to_rgba8();
    assert_eq!(
        (img.width(), img.height()),
        (width, height),
        "the reply's dimensions describe the PNG"
    );
    img
}

#[test]
#[ignore = "needs Mesa surfaceless EGL; run with --include-ignored"]
fn odd_window_at_fractional_capture_scale_has_no_gap_before_the_ring() {
    let _gl = gl::lock();
    let Some(renderer) = gl::surfaceless_renderer("odd window") else {
        return;
    };

    let temp = TempDir::new();
    let mut f = Fixture::with_config(super::config(&config_toml(false)));
    f.add_output(1, (2560, 1600));
    gl::install(&mut f, renderer);
    let ipc = f.start_ipc(temp.path());

    let client = f.add_client();
    map_odd_window(&mut f, client);
    let id = only_window_id(&mut f, &ipc);

    let img = capture_window(&mut f, &ipc, temp.path(), "odd-1.5.png", id, 1.5);
    // 1697x1057 logical (the window plus its 2px ring) at 1.5, rounded up.
    assert_eq!((img.width(), img.height()), (2546, 1586));

    assert_ring_hugs_content(
        &row(&img, img.height() / 2),
        ODD_RING_PX,
        ODD_CONTENT_W,
        "middle row at 1.5x",
    );
    assert_ring_hugs_content(
        &column(&img, img.width() / 2),
        ODD_RING_PX,
        ODD_CONTENT_H,
        "middle column at 1.5x",
    );

    // The frame's own half pixel: the last column and row are all the capture
    // has left over, and nothing else in it is transparent.
    for y in 0..img.height() {
        for x in 0..img.width() {
            let edge = x == img.width() - 1 || y == img.height() - 1;
            let clear = img.get_pixel(x, y)[3] == 0;
            assert_eq!(clear, edge, "pixel ({x}, {y}) is {:?}", img.get_pixel(x, y));
        }
    }

    // At an integral scale nothing is rounded up in the first place, so the
    // ring lands on the same edges the plain conversion gives and the capture
    // has no spare half pixel to leave clear.
    let img = capture_window(&mut f, &ipc, temp.path(), "odd-1.0.png", id, 1.0);
    assert_eq!((img.width(), img.height()), (1697, 1057));
    assert_ring_hugs_content(&row(&img, img.height() / 2), 2, 1693, "middle row at 1x");
    assert_ring_hugs_content(
        &column(&img, img.width() / 2),
        2,
        1053,
        "middle column at 1x",
    );

    gl::uninstall(&mut f);
}

#[test]
#[ignore = "needs Mesa surfaceless EGL; run with --include-ignored"]
fn square_ring_at_radius_zero_and_the_shadow_stays_outside() {
    let _gl = gl::lock();
    let Some(renderer) = gl::surfaceless_renderer("square ring") else {
        return;
    };

    let temp = TempDir::new();
    let mut f = Fixture::with_config(super::config(&config_toml(true)));
    f.add_output(1, (2560, 1600));
    gl::install(&mut f, renderer);
    let ipc = f.start_ipc(temp.path());

    // Even on both axes, so the chrome is integral at either capture scale and
    // only the corner shape is under test.
    let pixels = solid_bgra(504, 304, CONTENT_BGRA, false);
    let client = f.add_client();
    let _surface = map_window_shm(&mut f, client, "pixels", (504, 304), (504, 304), &pixels);
    let id = only_window_id(&mut f, &ipc);

    for (scale, file) in [(1.0, "square-1.0.png"), (1.5, "square-1.5.png")] {
        let img = capture_window(&mut f, &ipc, temp.path(), file, id, scale);
        let mid_row: Vec<Px> = row(&img, img.height() / 2)
            .iter()
            .map(|p| classify(*p))
            .collect();
        let mid_col: Vec<Px> = column(&img, img.width() / 2)
            .iter()
            .map(|p| classify(*p))
            .collect();
        let horizontal = runs_of(&mid_row, Px::Ring);
        let vertical = runs_of(&mid_col, Px::Ring);
        assert_eq!(horizontal.len(), 2, "{scale}x: ring runs across the row");
        assert_eq!(vertical.len(), 2, "{scale}x: ring runs down the column");
        let (left, right) = (horizontal[0].0 as u32, horizontal[1].1 as u32);
        let (top, bottom) = (vertical[0].0 as u32, vertical[1].1 as u32);

        // A quarter circle would leave the outermost corner pixel outside the
        // stroke, partly covered at best.
        for (x, y) in [(left, top), (right, bottom)] {
            let p = *img.get_pixel(x, y);
            assert_eq!(
                classify(p),
                Px::Ring,
                "{scale}x: corner ({x}, {y}) is {p:?}"
            );
            assert_eq!(p[3], 255, "{scale}x: corner ({x}, {y}) is {p:?}");
        }

        // The shadow grades from a square perimeter: diagonally past the corner
        // it is never stronger than straight out from the edge beside it.
        let diagonal = img.get_pixel(left - 1, top - 1)[3];
        let beside = img.get_pixel(left - 1, top + 1)[3];
        assert!(
            diagonal <= beside,
            "{scale}x: shadow bulges at the corner ({diagonal} > {beside})"
        );
        assert!(beside > 0, "{scale}x: no shadow outside the ring at all");
    }

    // Half-opaque red over nothing else: a mask that also shaded the window's
    // own interior would show up as extra alpha here.
    let reply = ipc_request(
        &mut f,
        &ipc,
        &Request::Opacity {
            window: Some(WindowSelector::Id(id)),
            value: Some(0.5),
        },
    );
    assert_eq!(reply, Ok(Response::Opacity(0.5)), "the opacity round-trips");

    let img = capture_window(&mut f, &ipc, temp.path(), "square-half.png", id, 1.0);
    let centre = *img.get_pixel(img.width() / 2, img.height() / 2);
    assert!(
        centre[0].abs_diff(128) <= 2
            && centre[1] == 0
            && centre[2] == 0
            && centre[3].abs_diff(128) <= 2,
        "the window centre at opacity 0.5 is {centre:?}"
    );

    gl::uninstall(&mut f);
}

#[test]
#[ignore = "needs Mesa surfaceless EGL; run with --include-ignored"]
fn live_frame_agrees_with_the_capture_at_fractional_output_scale() {
    let _gl = gl::lock();
    let Some(renderer) = gl::surfaceless_renderer("live frame") else {
        return;
    };

    let mut f = Fixture::with_config(super::config(&config_toml(false)));
    let output = f.add_output_scaled(1, (2560, 1600), 1.5);
    gl::install(&mut f, renderer);

    let client = f.add_client();
    map_odd_window(&mut f, client);
    // A live frame draws the open animation, which the capture path skips by
    // design — so this is the only test that has to run it out first.
    super::tick_until_settled(&mut f);

    // The frame the udev backend would submit, rendered offscreen: no damage
    // tracker, no cursor, no backend surface.
    let mut backend = f
        .state()
        .backend
        .take()
        .expect("the fixture has a renderer");
    let bytes = {
        let renderer = backend.renderer();
        let elements = crate::render::compose_frame(f.state(), renderer, &output, Vec::new());
        let refs: Vec<&crate::render::OutputRenderElements> = elements.iter().collect();
        crate::render::render_elements_to_rgba(
            renderer,
            Size::<i32, Physical>::from((2560, 1600)),
            Scale::from(1.5),
            &refs,
        )
    };
    f.state().backend = Some(backend);
    let img = RgbaImage::from_raw(2560, 1600, bytes.expect("render the live frame"))
        .expect("the frame fills the buffer");

    // The camera puts the window wherever it puts it; find it by its content.
    let (mut x0, mut y0, mut x1, mut y1) = (u32::MAX, u32::MAX, 0, 0);
    for y in 0..img.height() {
        for x in 0..img.width() {
            if classify(*img.get_pixel(x, y)) == Px::Content {
                x0 = x0.min(x);
                y0 = y0.min(y);
                x1 = x1.max(x);
                y1 = y1.max(y);
            }
        }
    }
    assert!(x0 < x1 && y0 < y1, "the live frame drew no window content");

    assert_ring_hugs_content(
        &row(&img, (y0 + y1) / 2),
        ODD_RING_PX,
        ODD_CONTENT_W,
        "live middle row",
    );
    assert_ring_hugs_content(
        &column(&img, (x0 + x1) / 2),
        ODD_RING_PX,
        ODD_CONTENT_H,
        "live middle column",
    );

    // Only a rendered frame creates per-output GPU state, so the fixture's
    // teardown baseline knows nothing about it; drop it the way an output
    // disconnect does.
    let output_name = output.name();
    f.state().render.remove_output(&output_name);
    gl::uninstall(&mut f);
}
