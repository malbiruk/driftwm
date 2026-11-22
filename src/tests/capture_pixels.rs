//! Screen capture through the wire protocols, rendered by a real GL context:
//! a wlr-screencopy region shows that region of the output, and both capture
//! protocols write a client's shm buffer where the buffer says, at its offset
//! and stride, failing the frame when the buffer doesn't fit, and stamp it on
//! the monotonic clock.
//!
//! `#[ignore]`d out of the default lane like the other pixel scenarios (needs
//! Mesa's surfaceless EGL, self-skipping when that is missing); each scenario
//! takes [`gl::lock`] first.

use driftwm::config::Action;
use smithay::output::Output;
use smithay::utils::{Logical, Physical, Rectangle, Size, Transform};
use wayland_client::protocol::wl_shm::Format;
use wayland_protocols::ext::image_copy_capture::v1::client::ext_image_copy_capture_frame_v1::FailureReason;
use wayland_protocols_wlr::screencopy::v1::client::zwlr_screencopy_frame_v1::ZwlrScreencopyFrameV1;

use super::capture_client::{CaptureBuffer, Record};
use super::client::ClientId;
use super::{Fixture, gl, map_window_shm};

const OUTPUT: (u16, u16) = (640, 400);
/// Inside the window, away from the output's origin.
const REGION: (i32, i32, i32, i32) = (100, 80, 200, 120);
/// Filled into every pool byte before a capture, to tell what it wrote.
const FILL: u8 = 0xAB;

const CONFIG: &str = "[background]\n\
                      type = \"none\"\n\
                      \n\
                      [decorations]\n\
                      default_mode = \"client\"\n\
                      shadow = false\n";

/// A window whose every texel differs from its neighbours, so two captures of
/// different parts of it can't agree by accident.
fn map_gradient_window(f: &mut Fixture, client: ClientId) {
    let (w, h) = (600, 360);
    let mut pixels = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        for x in 0..w {
            pixels.extend_from_slice(&[x as u8, y as u8, ((x >> 8) | ((y >> 8) << 4)) as u8, 255]);
        }
    }
    map_window_shm(f, client, "gradient", (w as u16, h as u16), (w, h), &pixels);
}

fn setup(test: &str) -> Option<(Fixture, Output, ClientId)> {
    let renderer = gl::surfaceless_renderer(test)?;
    let mut f = Fixture::with_config(super::config(CONFIG));
    let output = f.add_output(1, OUTPUT);
    gl::install(&mut f, renderer);
    let client = f.add_client();
    map_gradient_window(&mut f, client);
    super::tick_until_settled(&mut f);
    Some((f, output, client))
}

fn teardown(mut f: Fixture, output: &Output) {
    // A rendered frame leaves per-output GPU state the teardown baseline knows
    // nothing about; drop it the way an output disconnect does.
    f.state().render.remove_output(&output.name());
    gl::uninstall(&mut f);
}

/// Render one frame of `output` and fulfil the captures queued on it, as both
/// backends do right after drawing.
fn render_captures(f: &mut Fixture, output: &Output) {
    let mut backend = f
        .state()
        .backend
        .take()
        .expect("the fixture has a renderer");
    let renderer = backend.renderer();
    let elements = crate::render::compose_frame(f.state(), renderer, output, Vec::new());
    crate::render::render_screencopy(f.state(), renderer, output, &elements);
    crate::render::render_capture_frames(f.state(), renderer, output, &elements);
    f.state().backend = Some(backend);
}

/// The buffer size and stride a wlr frame asked for.
fn wlr_params(record: &Record) -> (i32, i32, i32) {
    record
        .lock()
        .unwrap()
        .shm
        .expect("the frame announced its shm buffer")
}

/// Copy a wlr frame into the buffer it asked for, placed `offset` bytes into a
/// pool with `tail` spare bytes after it.
fn wlr_copy(
    f: &mut Fixture,
    client: ClientId,
    frame: &ZwlrScreencopyFrameV1,
    record: &Record,
    offset: i32,
    tail: usize,
) -> CaptureBuffer {
    let (w, h, stride) = wlr_params(record);
    let pool_len = offset as usize + (stride * h) as usize + tail;
    let buffer =
        f.client(client)
            .capture_buffer(Format::Xrgb8888, (w, h), stride, offset, pool_len, FILL);
    frame.copy(&buffer.buffer);
    buffer
}

/// RGB of each pixel of a `w`x`h` little-endian xRGB/ARGB image inside `bytes`,
/// row by row from `offset`, rows `stride` apart.
fn rgb_pixels(bytes: &[u8], offset: usize, stride: usize, (w, h): (i32, i32)) -> Vec<[u8; 3]> {
    let mut px = Vec::with_capacity((w * h) as usize);
    for row in 0..h as usize {
        let start = offset + row * stride;
        for p in bytes[start..start + w as usize * 4].as_chunks::<4>().0 {
            px.push([p[2], p[1], p[0]]);
        }
    }
    px
}

/// The `(x, y, w, h)` rectangle of a tightly packed `full_w`-wide image.
fn crop(full: &[[u8; 3]], full_w: i32, (x, y, w, h): (i32, i32, i32, i32)) -> Vec<[u8; 3]> {
    let mut px = Vec::with_capacity((w * h) as usize);
    for row in y..y + h {
        let start = (row * full_w + x) as usize;
        px.extend_from_slice(&full[start..start + w as usize]);
    }
    px
}

/// Whether every byte outside the `w`x`h` image at `offset`/`stride` still
/// holds [`FILL`].
fn slack_untouched(bytes: &[u8], offset: usize, stride: usize, (w, h): (i32, i32)) -> bool {
    let row_bytes = w as usize * 4;
    bytes.iter().enumerate().all(|(i, &b)| {
        let inside = i >= offset && {
            let (row, col) = ((i - offset) / stride, (i - offset) % stride);
            row < h as usize && col < row_bytes
        };
        inside || b == FILL
    })
}

/// Capture [`REGION`] and the whole output over wlr-screencopy, on an output
/// at `scale` turned by `transform`, and check the region's buffer is the
/// stretch of the whole output's buffer the region covers. Both buffers are
/// in scanout orientation, so the stretch is the region turned back.
fn region_capture_matches_the_output(test: &str, scale: f64, transform: Transform) {
    let _gl = gl::lock();
    let Some(renderer) = gl::surfaceless_renderer(test) else {
        return;
    };
    let mut f = Fixture::with_config(super::config(CONFIG));
    let output = f.add_output_scaled(1, OUTPUT, scale);
    output.change_current_state(None, Some(transform), None, None);
    gl::install(&mut f, renderer);
    let client = f.add_client();
    map_gradient_window(&mut f, client);
    super::tick_until_settled(&mut f);
    let name = output.name();

    let mode = Size::<i32, Physical>::from((i32::from(OUTPUT.0), i32::from(OUTPUT.1)));
    let turned = transform.transform_size(mode);
    let (x, y, w, h) = REGION;
    let in_output = Rectangle::<i32, Logical>::new((x, y).into(), (w, h).into())
        .to_physical_precise_round(scale)
        .intersection(Rectangle::from_size(turned))
        .expect("the region is on the output");
    let scanout = transform.invert().transform_rect_in(in_output, &turned);

    let globals = f.client(client).bind_capture_globals();
    f.roundtrip(client);
    let (full, full_record) = f.client(client).wlr_capture(&globals, &name, None);
    let (region, region_record) = f.client(client).wlr_capture(&globals, &name, Some(REGION));
    f.roundtrip(client);
    let size = (scanout.size.w, scanout.size.h);
    assert_eq!(
        wlr_params(&region_record),
        (size.0, size.1, size.0 * 4),
        "the region asks for a buffer of its own size"
    );
    let full_buffer = wlr_copy(&mut f, client, &full, &full_record, 0, 0);
    let region_buffer = wlr_copy(&mut f, client, &region, &region_record, 0, 0);
    f.roundtrip(client);
    render_captures(&mut f, &output);
    f.roundtrip(client);
    assert!(
        full_record.lock().unwrap().ready,
        "the full capture completed"
    );
    assert!(
        region_record.lock().unwrap().ready,
        "the region capture completed"
    );

    let full_px = rgb_pixels(
        &full_buffer.pool_bytes(),
        0,
        mode.w as usize * 4,
        (mode.w, mode.h),
    );
    let region_px = rgb_pixels(&region_buffer.pool_bytes(), 0, size.0 as usize * 4, size);
    let stretch = crop(
        &full_px,
        mode.w,
        (scanout.loc.x, scanout.loc.y, size.0, size.1),
    );
    assert_ne!(
        stretch,
        crop(&full_px, mode.w, (0, 0, size.0, size.1)),
        "the scenario needs the region to look different from the buffer's origin"
    );
    assert!(
        region_px == stretch,
        "the region capture holds the output's pixels at the region"
    );

    teardown(f, &output);
}

#[test]
#[ignore = "needs Mesa surfaceless EGL; run with --include-ignored"]
fn a_wlr_region_capture_shows_that_region_of_the_output() {
    region_capture_matches_the_output("wlr region capture", 1.0, Transform::Normal);
}

#[test]
#[ignore = "needs Mesa surfaceless EGL; run with --include-ignored"]
fn a_wlr_region_capture_at_a_fractional_scale_shows_that_region() {
    region_capture_matches_the_output("wlr region at 1.5x", 1.5, Transform::Normal);
}

#[test]
#[ignore = "needs Mesa surfaceless EGL; run with --include-ignored"]
fn a_wlr_region_capture_on_a_rotated_output_shows_that_region() {
    region_capture_matches_the_output("wlr region rotated", 1.0, Transform::_90);
}

#[test]
#[ignore = "needs Mesa surfaceless EGL; run with --include-ignored"]
fn a_wlr_capture_lands_at_its_buffer_offset_in_a_larger_pool() {
    let _gl = gl::lock();
    let Some((mut f, output, client)) = setup("wlr pool offset") else {
        return;
    };
    let name = output.name();

    let globals = f.client(client).bind_capture_globals();
    f.roundtrip(client);
    let (tight, tight_record) = f.client(client).wlr_capture(&globals, &name, Some(REGION));
    let (placed, placed_record) = f.client(client).wlr_capture(&globals, &name, Some(REGION));
    f.roundtrip(client);
    let tight_buffer = wlr_copy(&mut f, client, &tight, &tight_record, 0, 0);
    let placed_buffer = wlr_copy(&mut f, client, &placed, &placed_record, 256, 64);
    f.roundtrip(client);
    render_captures(&mut f, &output);
    f.roundtrip(client);
    assert!(
        placed_record.lock().unwrap().ready,
        "the offset capture completed"
    );

    let size = (REGION.2, REGION.3);
    let stride = REGION.2 as usize * 4;
    let bytes = placed_buffer.pool_bytes();
    assert_eq!(
        rgb_pixels(&bytes, 256, stride, size),
        rgb_pixels(&tight_buffer.pool_bytes(), 0, stride, size),
        "the capture starts at the buffer's offset"
    );
    assert!(
        slack_untouched(&bytes, 256, stride, size),
        "nothing outside the buffer was written"
    );

    teardown(f, &output);
}

#[test]
#[ignore = "needs Mesa surfaceless EGL; run with --include-ignored"]
fn an_ext_capture_follows_the_buffers_offset_and_stride() {
    let _gl = gl::lock();
    let Some((mut f, output, client)) = setup("ext offset and stride") else {
        return;
    };
    let name = output.name();

    let globals = f.client(client).bind_capture_globals();
    f.roundtrip(client);
    let (reference, reference_record) = f.client(client).wlr_capture(&globals, &name, None);
    let (session, session_record) = f.client(client).ext_capture_session(&globals, &name);
    f.roundtrip(client);
    let (w, h, _) = session_record
        .lock()
        .unwrap()
        .shm
        .expect("the session announced its buffer size");
    let reference_buffer = wlr_copy(&mut f, client, &reference, &reference_record, 0, 0);

    // A padded layout in a pool with slack on both sides, in Argb8888: the
    // other format the session advertises.
    let (offset, stride) = (128, w * 4 + 64);
    let pool_len = (offset + stride * h) as usize + 32;
    let buffer =
        f.client(client)
            .capture_buffer(Format::Argb8888, (w, h), stride, offset, pool_len, FILL);
    let (_frame, frame_record) = f.client(client).ext_capture(&session, &buffer.buffer);
    f.roundtrip(client);
    render_captures(&mut f, &output);
    f.roundtrip(client);
    assert!(
        frame_record.lock().unwrap().ready,
        "the ext capture completed"
    );

    let bytes = buffer.pool_bytes();
    let (offset, stride) = (offset as usize, stride as usize);
    assert!(
        rgb_pixels(&bytes, offset, stride, (w, h))
            == rgb_pixels(&reference_buffer.pool_bytes(), 0, w as usize * 4, (w, h)),
        "each row lands at offset + row * stride"
    );
    assert!(
        slack_untouched(&bytes, offset, stride, (w, h)),
        "row padding and pool slack stay untouched"
    );

    teardown(f, &output);
}

/// A recorder lines its audio up against the frame's stamp, so both protocols
/// stamp on `CLOCK_MONOTONIC`, the clock driftwm advertises.
#[test]
#[ignore = "needs Mesa surfaceless EGL; run with --include-ignored"]
fn a_captured_frame_is_stamped_on_the_monotonic_clock() {
    let _gl = gl::lock();
    let Some((mut f, output, client)) = setup("capture timestamp") else {
        return;
    };
    let name = output.name();

    let globals = f.client(client).bind_capture_globals();
    f.roundtrip(client);
    let (wlr, wlr_record) = f.client(client).wlr_capture(&globals, &name, None);
    let (session, session_record) = f.client(client).ext_capture_session(&globals, &name);
    f.roundtrip(client);
    let (w, h, _) = session_record
        .lock()
        .unwrap()
        .shm
        .expect("the session announced its buffer size");
    let _wlr_buffer = wlr_copy(&mut f, client, &wlr, &wlr_record, 0, 0);
    let ext_buffer = f.client(client).capture_buffer(
        Format::Argb8888,
        (w, h),
        w * 4,
        0,
        (w * 4 * h) as usize,
        FILL,
    );
    let (_frame, ext_record) = f.client(client).ext_capture(&session, &ext_buffer.buffer);
    f.roundtrip(client);
    render_captures(&mut f, &output);
    f.roundtrip(client);

    let now = crate::state::monotonic_now();
    for (protocol, record) in [("wlr", &wlr_record), ("ext", &ext_record)] {
        let stamp = record
            .lock()
            .unwrap()
            .presented
            .unwrap_or_else(|| panic!("the {protocol} frame was stamped"));
        assert!(
            now.abs_diff(stamp) < std::time::Duration::from_secs(2),
            "{protocol} stamped {stamp:?}, monotonic now is {now:?}"
        );
    }

    teardown(f, &output);
}

#[test]
#[ignore = "needs Mesa surfaceless EGL; run with --include-ignored"]
fn an_ext_capture_into_a_buffer_that_does_not_fit_fails_on_constraints() {
    let _gl = gl::lock();
    let Some((mut f, output, client)) = setup("ext buffer constraints") else {
        return;
    };
    let name = output.name();

    let globals = f.client(client).bind_capture_globals();
    f.roundtrip(client);
    // A session each, so neither frame waits on damage behind the other.
    let (narrow_session, session_record) = f.client(client).ext_capture_session(&globals, &name);
    let (foreign_session, _) = f.client(client).ext_capture_session(&globals, &name);
    f.roundtrip(client);
    let (w, h, _) = session_record.lock().unwrap().shm.unwrap();

    // One texel too narrow, and a format wl_shm takes but the session never
    // advertised.
    let pool_len = (w * 4 * h) as usize;
    let narrow =
        f.client(client)
            .capture_buffer(Format::Xrgb8888, (w - 1, h), w * 4, 0, pool_len, FILL);
    let foreign =
        f.client(client)
            .capture_buffer(Format::Abgr8888, (w, h), w * 4, 0, pool_len, FILL);
    let (_narrow_frame, narrow_record) = f
        .client(client)
        .ext_capture(&narrow_session, &narrow.buffer);
    let (_foreign_frame, foreign_record) = f
        .client(client)
        .ext_capture(&foreign_session, &foreign.buffer);
    f.roundtrip(client);
    render_captures(&mut f, &output);
    f.roundtrip(client);

    for (what, record, buffer) in [
        ("too narrow", &narrow_record, &narrow),
        ("unadvertised format", &foreign_record, &foreign),
    ] {
        let record = record.lock().unwrap();
        assert!(!record.ready, "a {what} buffer is not reported ready");
        assert_eq!(
            record.failure_reason,
            Some(FailureReason::BufferConstraints),
            "a {what} buffer fails on its constraints"
        );
        assert!(
            buffer.pool_bytes().iter().all(|&b| b == FILL),
            "nothing is written into a {what} buffer"
        );
    }

    teardown(f, &output);
}

/// The whole output over wlr-screencopy, as RGB rows.
fn capture_output(f: &mut Fixture, client: ClientId, output: &Output) -> Vec<[u8; 3]> {
    let globals = f.client(client).bind_capture_globals();
    f.roundtrip(client);
    let (frame, record) = f.client(client).wlr_capture(&globals, &output.name(), None);
    f.roundtrip(client);
    let buffer = wlr_copy(f, client, &frame, &record, 0, 0);
    f.roundtrip(client);
    render_captures(f, output);
    f.roundtrip(client);
    assert!(record.lock().unwrap().ready, "the capture completed");
    let (w, h, stride) = wlr_params(&record);
    rgb_pixels(&buffer.pool_bytes(), 0, stride as usize, (w, h))
}

/// How many times the colour changes between neighbouring pixels along `row`.
fn transitions(pixels: &[[u8; 3]], row: i32) -> usize {
    let start = (row * i32::from(OUTPUT.0)) as usize;
    pixels[start..start + usize::from(OUTPUT.0)]
        .windows(2)
        .filter(|pair| pair[0] != pair[1])
        .count()
}

#[test]
#[ignore = "needs Mesa surfaceless EGL; run with --include-ignored"]
fn an_obscured_window_is_a_mosaic_in_screencopy_and_restores_when_toggled_back() {
    let _gl = gl::lock();
    let Some((mut f, output, client)) = setup("obscure in screencopy") else {
        return;
    };
    let row = i32::from(OUTPUT.1) / 2;

    let sharp = capture_output(&mut f, client, &output);
    assert!(
        transitions(&sharp, row) > 400,
        "the gradient changes colour at nearly every pixel"
    );

    f.state().execute_action(&Action::ToggleObscure);
    let covered = capture_output(&mut f, client, &output);
    let blocks = transitions(&covered, row);
    assert!(
        blocks > 0 && blocks * 8 < transitions(&sharp, row),
        "an obscured row changes colour once per mosaic cell, not per pixel: {blocks}"
    );
    assert_ne!(covered, sharp, "the window picture changed");

    f.state().execute_action(&Action::ToggleObscure);
    assert_eq!(
        capture_output(&mut f, client, &output),
        sharp,
        "toggling back restores the sharp picture"
    );

    teardown(f, &output);
}
