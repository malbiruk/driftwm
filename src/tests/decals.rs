//! Decals: images pinned to the canvas, placed and removed over IPC and saved
//! across restarts. Drawing needs a live renderer and is not covered here;
//! placement, hit-testing, bounds and persistence are.

use std::path::{Path, PathBuf};

use driftwm::decals;
use smithay::utils::{Logical, Point};

use super::Fixture;
use super::input_backend::{FakeDevice, pointer_to};
use super::real::TempDir;
use crate::ipc::dispatch;
use crate::ipc::protocol::{DecalRequest, Request, Response};

fn png(dir: &Path, name: &str, w: u32, h: u32) -> PathBuf {
    let path = dir.join(name);
    image::RgbaImage::new(w, h).save(&path).unwrap();
    path
}

fn add(f: &mut Fixture, path: &Path, at: Option<(f64, f64)>, scale: f64) -> Result<u64, String> {
    let request = Request::Decal(DecalRequest::Add {
        path: path.to_string_lossy().into_owned(),
        at,
        scale: Some(scale),
        fit: None,
    });
    match dispatch(request, f.state())? {
        Response::Decal { id } => Ok(id),
        other => panic!("unexpected reply {other:?}"),
    }
}

fn remove(f: &mut Fixture, id: Option<u64>) -> Result<(), String> {
    dispatch(Request::Decal(DecalRequest::Remove { id }), f.state()).map(|_| ())
}

fn listed(f: &mut Fixture) -> Vec<(u64, [f64; 2], f64)> {
    match dispatch(Request::Decal(DecalRequest::List), f.state()).unwrap() {
        Response::Decals(list) => list
            .into_iter()
            .map(|d| (d.id, d.position, d.scale))
            .collect(),
        other => panic!("unexpected reply {other:?}"),
    }
}

fn fixture() -> Fixture {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    f
}

#[test]
fn a_placed_decal_is_listed_with_its_position_and_scale() {
    let tmp = TempDir::new();
    let mut f = fixture();
    let img = png(tmp.path(), "a.png", 64, 32);

    let id = add(&mut f, &img, Some((100.0, -50.0)), 2.0).unwrap();

    assert_eq!(listed(&mut f), vec![(id, [100.0, -50.0], 2.0)]);
}

fn add_fit(f: &mut Fixture, path: &Path, fit: f64) -> Result<u64, String> {
    let request = Request::Decal(DecalRequest::Add {
        path: path.to_string_lossy().into_owned(),
        at: Some((0.0, 0.0)),
        scale: None,
        fit: Some(fit),
    });
    match dispatch(request, f.state())? {
        Response::Decal { id } => Ok(id),
        other => panic!("unexpected reply {other:?}"),
    }
}

#[test]
fn fit_sizes_the_longest_side_whatever_the_image() {
    let tmp = TempDir::new();
    let mut f = fixture();
    let wide = png(tmp.path(), "wide.png", 400, 100);
    let tall = png(tmp.path(), "tall.png", 50, 200);

    add_fit(&mut f, &wide, 256.0).unwrap();
    add_fit(&mut f, &tall, 256.0).unwrap();

    let scales: Vec<f64> = listed(&mut f).iter().map(|d| d.2).collect();
    assert_eq!(scales, vec![256.0 / 400.0, 256.0 / 200.0]);
}

#[test]
fn a_scale_and_a_fit_together_are_refused() {
    let tmp = TempDir::new();
    let mut f = fixture();
    let img = png(tmp.path(), "a.png", 8, 8);
    let request = Request::Decal(DecalRequest::Add {
        path: img.to_string_lossy().into_owned(),
        at: Some((0.0, 0.0)),
        scale: Some(2.0),
        fit: Some(64.0),
    });

    assert!(dispatch(request, f.state()).is_err());
    assert!(listed(&mut f).is_empty());
}

#[test]
fn a_fit_that_leaves_the_scale_out_of_range_is_refused() {
    let tmp = TempDir::new();
    let mut f = fixture();
    let img = png(tmp.path(), "a.png", 8, 8);

    assert!(add_fit(&mut f, &img, 0.0).is_err());
    assert!(add_fit(&mut f, &img, 1e6).is_err());
}

#[test]
fn ids_are_not_reused_after_a_removal() {
    let tmp = TempDir::new();
    let mut f = fixture();
    let img = png(tmp.path(), "a.png", 8, 8);

    let first = add(&mut f, &img, Some((0.0, 0.0)), 1.0).unwrap();
    remove(&mut f, Some(first)).unwrap();
    let second = add(&mut f, &img, Some((0.0, 0.0)), 1.0).unwrap();

    assert_ne!(first, second);
}

#[test]
fn a_decal_with_no_position_lands_centered_under_the_pointer() {
    let tmp = TempDir::new();
    let mut f = fixture();
    let img = png(tmp.path(), "a.png", 8, 8);
    pointer_to(
        &mut f,
        &FakeDevice::mouse(),
        Point::<f64, Logical>::from((300.0, 200.0)),
    );

    add(&mut f, &img, None, 1.0).unwrap();

    // Canvas is Y-down internally, the reported position Y-up.
    assert_eq!(listed(&mut f)[0].1, [300.0, -200.0]);
}

#[test]
fn removing_with_no_id_takes_the_topmost_decal_under_the_pointer() {
    let tmp = TempDir::new();
    let mut f = fixture();
    let img = png(tmp.path(), "a.png", 100, 100);
    let under = add(&mut f, &img, Some((0.0, 0.0)), 1.0).unwrap();
    let over = add(&mut f, &img, Some((20.0, 0.0)), 1.0).unwrap();
    pointer_to(
        &mut f,
        &FakeDevice::mouse(),
        Point::<f64, Logical>::from((10.0, 0.0)),
    );

    remove(&mut f, None).unwrap();
    assert_eq!(
        listed(&mut f).iter().map(|d| d.0).collect::<Vec<_>>(),
        vec![under],
        "the later-placed one is on top, so it goes first (id {over})"
    );

    remove(&mut f, None).unwrap();
    assert!(listed(&mut f).is_empty());
}

#[test]
fn removing_with_the_pointer_off_every_decal_is_an_error() {
    let tmp = TempDir::new();
    let mut f = fixture();
    let img = png(tmp.path(), "a.png", 10, 10);
    add(&mut f, &img, Some((0.0, 0.0)), 1.0).unwrap();
    pointer_to(
        &mut f,
        &FakeDevice::mouse(),
        Point::<f64, Logical>::from((500.0, 500.0)),
    );

    assert_eq!(
        remove(&mut f, None),
        Err("no decal under the pointer".to_string())
    );
    assert_eq!(listed(&mut f).len(), 1);
}

#[test]
fn an_unknown_id_is_an_error() {
    let mut f = fixture();
    assert_eq!(
        remove(&mut f, Some(9)),
        Err("no decal with id 9".to_string())
    );
}

#[test]
fn files_the_renderer_could_not_load_are_refused_up_front() {
    let tmp = TempDir::new();
    let mut f = fixture();

    assert!(add(&mut f, Path::new("a.png"), Some((0.0, 0.0)), 1.0).is_err());
    assert!(
        add(
            &mut f,
            &tmp.path().join("missing.png"),
            Some((0.0, 0.0)),
            1.0
        )
        .is_err()
    );
    let text = tmp.path().join("notes.png");
    std::fs::write(&text, "not an image").unwrap();
    assert!(add(&mut f, &text, Some((0.0, 0.0)), 1.0).is_err());
    assert!(add(&mut f, tmp.path(), Some((0.0, 0.0)), 1.0).is_err());
    assert!(listed(&mut f).is_empty());
}

#[test]
fn an_oversized_image_is_refused() {
    let tmp = TempDir::new();
    let mut f = fixture();
    let wide = png(tmp.path(), "wide.png", 8193, 1);

    assert!(add(&mut f, &wide, Some((0.0, 0.0)), 1.0).is_err());
}

#[test]
fn out_of_range_positions_and_scales_are_refused() {
    let tmp = TempDir::new();
    let mut f = fixture();
    let img = png(tmp.path(), "a.png", 8, 8);

    assert!(add(&mut f, &img, Some((f64::NAN, 0.0)), 1.0).is_err());
    assert!(add(&mut f, &img, Some((1e12, 0.0)), 1.0).is_err());
    assert!(add(&mut f, &img, Some((0.0, 0.0)), 0.0).is_err());
    assert!(add(&mut f, &img, Some((0.0, 0.0)), 1e3).is_err());
}

#[test]
fn the_registry_takes_no_decal_past_its_cap() {
    let tmp = TempDir::new();
    let mut f = fixture();
    let img = png(tmp.path(), "a.png", 4, 4);

    for _ in 0..decals::MAX_DECALS {
        add(&mut f, &img, Some((0.0, 0.0)), 1.0).unwrap();
    }
    assert!(add(&mut f, &img, Some((0.0, 0.0)), 1.0).is_err());
}

#[test]
fn decals_survive_a_restart() {
    let tmp = TempDir::new();
    let img = png(tmp.path(), "a.png", 16, 16);
    let file = tmp.path().join("decals.json");

    let mut f = fixture();
    f.state().decals.path = Some(file.clone());
    let kept = add(&mut f, &img, Some((7.0, -9.0)), 0.5).unwrap();
    let dropped = add(&mut f, &img, Some((1.0, 1.0)), 1.0).unwrap();
    remove(&mut f, Some(dropped)).unwrap();

    let mut restarted = fixture();
    restarted.state().decals.path = Some(file);
    restarted.state().load_decals();

    assert_eq!(listed(&mut restarted), vec![(kept, [7.0, -9.0], 0.5)]);
    let next = add(&mut restarted, &img, Some((0.0, 0.0)), 1.0).unwrap();
    assert!(next > kept, "ids keep counting past the saved ones");
}

#[test]
fn a_decal_whose_file_vanished_is_kept_but_cannot_be_hit() {
    let tmp = TempDir::new();
    let img = png(tmp.path(), "a.png", 16, 16);
    let file = tmp.path().join("decals.json");
    let mut f = fixture();
    f.state().decals.path = Some(file.clone());
    add(&mut f, &img, Some((0.0, 0.0)), 1.0).unwrap();
    std::fs::remove_file(&img).unwrap();

    let mut restarted = fixture();
    restarted.state().decals.path = Some(file);
    restarted.state().load_decals();
    pointer_to(
        &mut restarted,
        &FakeDevice::mouse(),
        Point::<f64, Logical>::from((0.0, 0.0)),
    );

    assert_eq!(listed(&mut restarted).len(), 1);
    assert!(remove(&mut restarted, None).is_err());
}
