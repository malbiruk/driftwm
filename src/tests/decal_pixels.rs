//! Decals drawn by a real GL context: where the image lands, how its alpha
//! blends, and how it scales, read back from the compositor's own capture path.
//!
//! `#[ignore]`d out of the default lane (needs Mesa's surfaceless EGL,
//! self-skipping when that is missing); each scenario takes [`gl::lock`] first.

use std::path::Path;

use image::{Rgba, RgbaImage};
use smithay::utils::Rectangle;

use super::real::TempDir;
use super::{Fixture, gl};
use crate::ipc::dispatch;
use crate::ipc::protocol::{DecalRequest, Request};

const SIDE: u32 = 20;
const RED: [u8; 4] = [255, 0, 0, 255];
/// Straight alpha, as the source PNG stores it.
const HALF_GREY: [u8; 4] = [100, 100, 100, 128];
/// [`HALF_GREY`] premultiplied, which is how a capture reads back: 100 * 128/255.
const HALF_GREY_PREMULTIPLIED: [u8; 4] = [50, 50, 50, 128];

/// Left half opaque red, right half half-transparent grey.
fn write_test_image(path: &Path) {
    let mut img = RgbaImage::new(SIDE, SIDE);
    for (x, _, px) in img.enumerate_pixels_mut() {
        *px = Rgba(if x < SIDE / 2 { RED } else { HALF_GREY });
    }
    img.save(path).unwrap();
}

fn place(f: &mut Fixture, path: &Path, scale: f64) {
    dispatch(
        Request::Decal(DecalRequest::Add {
            path: path.to_string_lossy().into_owned(),
            at: Some((0.0, 0.0)),
            scale: Some(scale),
            fit: None,
        }),
        f.state(),
    )
    .unwrap();
}

/// Capture the canvas square `[-20, 20)` on both axes at one pixel per unit,
/// over a transparent background.
fn capture(f: &mut Fixture, path: &Path) -> RgbaImage {
    let region = Rectangle::new((-20, -20).into(), (40, 40).into());
    let mut backend = f
        .state()
        .backend
        .take()
        .expect("the fixture has a renderer");
    let captured = crate::render::capture_region_to_png(
        f.state(),
        backend.renderer(),
        region,
        1.0,
        false,
        None,
        path,
    );
    f.state().backend = Some(backend);
    captured.expect("capture the region");
    image::open(path).expect("open the capture").to_rgba8()
}

/// The pixel at canvas point `(x, y)` of a [`capture`].
fn at(img: &RgbaImage, x: i32, y: i32) -> [u8; 4] {
    img.get_pixel((x + 20) as u32, (y + 20) as u32).0
}

fn near(got: [u8; 4], want: [u8; 4], tol: i16) -> bool {
    got.iter()
        .zip(want)
        .all(|(g, w)| (*g as i16 - w as i16).abs() <= tol)
}

#[test]
#[ignore = "needs Mesa surfaceless EGL; run with --include-ignored"]
fn a_decal_lands_centered_on_its_canvas_point_and_blends_premultiplied() {
    let _gl = gl::lock();
    let Some(renderer) = gl::surfaceless_renderer("decal pixels") else {
        return;
    };
    let temp = TempDir::new();
    let image_path = temp.path().join("decal.png");
    write_test_image(&image_path);

    let mut f = Fixture::with_config(super::config("[background]\ntype = \"none\"\n"));
    f.add_output(1, (800, 600));
    gl::install(&mut f, renderer);
    place(&mut f, &image_path, 1.0);

    let img = capture(&mut f, &temp.path().join("capture.png"));

    assert!(near(at(&img, -5, 0), RED, 2), "left half is opaque red");
    assert_eq!(at(&img, -15, 0)[3], 0, "left of the decal is empty");
    assert_eq!(at(&img, 15, 0)[3], 0, "right of the decal is empty");
    assert_eq!(at(&img, 0, -15)[3], 0, "above the decal is empty");
    assert_eq!(at(&img, 0, 15)[3], 0, "below the decal is empty");
    let soft = at(&img, 5, 0);
    assert!(
        (soft[3] as i16 - 128).abs() <= 2,
        "the right half keeps its alpha, got {soft:?}"
    );
    assert!(
        near(soft, HALF_GREY_PREMULTIPLIED, 3),
        "premultiplied on upload; straight alpha would read back as {HALF_GREY:?}, got {soft:?}"
    );

    gl::uninstall(&mut f);
}

#[test]
#[ignore = "needs Mesa surfaceless EGL; run with --include-ignored"]
fn a_flipped_decal_is_mirrored_left_to_right_in_place() {
    let _gl = gl::lock();
    let Some(renderer) = gl::surfaceless_renderer("decal flip") else {
        return;
    };
    let temp = TempDir::new();
    let image_path = temp.path().join("decal.png");
    write_test_image(&image_path);

    let mut f = Fixture::with_config(super::config("[background]\ntype = \"none\"\n"));
    f.add_output(1, (800, 600));
    gl::install(&mut f, renderer);
    place(&mut f, &image_path, 1.0);
    dispatch(
        Request::Decal(DecalRequest::Flip { id: Some(1) }),
        f.state(),
    )
    .unwrap();

    let img = capture(&mut f, &temp.path().join("capture.png"));

    assert!(
        near(at(&img, 5, 0), RED, 2),
        "the red half is now on the right"
    );
    assert!(
        near(at(&img, -5, 0), HALF_GREY_PREMULTIPLIED, 3),
        "and the soft half on the left, got {:?}",
        at(&img, -5, 0)
    );
    assert_eq!(at(&img, -15, 0)[3], 0, "the footprint is unchanged");
    assert_eq!(at(&img, 15, 0)[3], 0, "the footprint is unchanged");

    gl::uninstall(&mut f);
}

#[test]
#[ignore = "needs Mesa surfaceless EGL; run with --include-ignored"]
fn a_flipped_decal_keeps_its_footprint_at_other_scales() {
    let _gl = gl::lock();
    let Some(renderer) = gl::surfaceless_renderer("decal flip scaled") else {
        return;
    };
    let temp = TempDir::new();
    let image_path = temp.path().join("decal.png");
    write_test_image(&image_path);

    let mut f = Fixture::with_config(super::config("[background]\ntype = \"none\"\n"));
    f.add_output(1, (800, 600));
    gl::install(&mut f, renderer);
    place(&mut f, &image_path, 0.5);
    dispatch(
        Request::Decal(DecalRequest::Flip { id: Some(1) }),
        f.state(),
    )
    .unwrap();

    let img = capture(&mut f, &temp.path().join("capture.png"));
    // The 20 px image at 0.5 is 10 px wide, centered: x in [-5, 5).
    assert!(
        near(at(&img, 2, 0), RED, 2),
        "red half on the right, got {:?}",
        at(&img, 2, 0)
    );
    assert!(
        near(at(&img, -3, 0), HALF_GREY_PREMULTIPLIED, 4),
        "soft half on the left, got {:?}",
        at(&img, -3, 0)
    );
    assert_eq!(at(&img, -8, 0)[3], 0);
    assert_eq!(at(&img, 8, 0)[3], 0);

    gl::uninstall(&mut f);
}

#[test]
#[ignore = "needs Mesa surfaceless EGL; run with --include-ignored"]
fn scale_multiplies_the_image_size_about_its_center() {
    let _gl = gl::lock();
    let Some(renderer) = gl::surfaceless_renderer("decal scale") else {
        return;
    };
    let temp = TempDir::new();
    let image_path = temp.path().join("decal.png");
    write_test_image(&image_path);

    let mut f = Fixture::with_config(super::config("[background]\ntype = \"none\"\n"));
    f.add_output(1, (800, 600));
    gl::install(&mut f, renderer);
    place(&mut f, &image_path, 2.0);

    let img = capture(&mut f, &temp.path().join("capture.png"));

    assert!(
        near(at(&img, -15, 0), RED, 2),
        "the red half now reaches x=-15"
    );
    assert!(
        near(at(&img, -2, 0), RED, 2),
        "and up to the center line, so the whole image is stretched, not just a corner of it"
    );
    assert!(
        near(at(&img, 2, 0), HALF_GREY_PREMULTIPLIED, 3),
        "the soft half starts at the center line, got {:?}",
        at(&img, 2, 0)
    );
    assert!(at(&img, 15, 0)[3] > 0, "the soft half reaches x=15");
    assert!(
        at(&img, 0, 19)[3] > 0,
        "the image is 40 tall, so it reaches y=19"
    );

    gl::uninstall(&mut f);
}

#[test]
#[ignore = "needs Mesa surfaceless EGL; run with --include-ignored"]
fn a_window_draws_over_a_decal_that_extends_past_it() {
    use super::render_pixels::solid_bgra;

    let _gl = gl::lock();
    let Some(renderer) = gl::surfaceless_renderer("decal below window") else {
        return;
    };
    let temp = TempDir::new();
    let image_path = temp.path().join("green.png");
    RgbaImage::from_pixel(200, 200, Rgba([0, 255, 0, 255]))
        .save(&image_path)
        .unwrap();

    let mut f = Fixture::with_config(super::config(
        "[background]\ntype = \"none\"\n[decorations]\ndefault_mode = \"client\"\nborder_width = 0\nshadow = false\n",
    ));
    f.add_output(1, (800, 600));
    gl::install(&mut f, renderer);

    let blue_bgra = [255, 0, 0, 255];
    let client = f.add_client();
    super::map_window_shm(
        &mut f,
        client,
        "pixels",
        (100, 100),
        (100, 100),
        &solid_bgra(100, 100, blue_bgra, false),
    );
    super::tick_until_settled(&mut f);
    let window = super::window_by_app_id(&mut f, "pixels").expect("the window mapped");
    let loc = f.state().stage.position_of(&window).expect("on the stage");
    let center = (loc.x + 50, loc.y + 50);

    dispatch(
        Request::Decal(DecalRequest::Add {
            path: image_path.to_string_lossy().into_owned(),
            at: Some((center.0 as f64, -(center.1 as f64))),
            scale: Some(1.0),
            fit: None,
        }),
        f.state(),
    )
    .unwrap();

    let region = Rectangle::new((center.0 - 100, center.1 - 100).into(), (200, 200).into());
    let path = temp.path().join("capture.png");
    let mut backend = f
        .state()
        .backend
        .take()
        .expect("the fixture has a renderer");
    let captured = crate::render::capture_region_to_png(
        f.state(),
        backend.renderer(),
        region,
        1.0,
        true,
        None,
        &path,
    );
    f.state().backend = Some(backend);
    captured.expect("capture the region");
    let img = image::open(&path).unwrap().to_rgba8();

    let blue = [0, 0, 255, 255];
    let green = [0, 255, 0, 255];
    assert!(
        near(img.get_pixel(100, 100).0, blue, 2),
        "the window covers the decal"
    );
    assert!(
        near(img.get_pixel(100, 20).0, green, 2),
        "the decal shows around the window"
    );

    gl::uninstall(&mut f);
}
