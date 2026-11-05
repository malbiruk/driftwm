//! Idle-frame damage of a real window drawn through the compositor's own
//! element path and smithay's damage tracker, on a surfaceless GL context.
//!
//! `#[ignore]`d out of the default lane (needs Mesa's surfaceless EGL,
//! self-skipping when that is missing); each scenario takes [`gl::lock`] first.

use smithay::backend::renderer::damage::OutputDamageTracker;
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::output::Output;
use smithay::utils::{Logical, Physical, Point, Rectangle, Transform};

use super::{Fixture, gl, map_window, window_by_app_id};

const OUTPUT: (i32, i32) = (1920, 1080);
const WINDOW: (u16, u16) = (400, 300);

/// Nothing opaque sits beneath the window, so its own opaque regions, rounded
/// corner bands included, are the only ones the tracker sees. The radius is pinned
/// because the band height follows it: a changed default could stop the bands
/// collapsing at the zoom under test and leave nothing to catch. The shadow is off
/// so it can't add damage of its own.
const CONFIG: &str = r#"
[background]
type = "none"

[decorations]
corner_radius = 10
shadow = false
"#;

fn compose(f: &mut Fixture, output: &Output) -> Vec<crate::render::OutputRenderElements> {
    let mut backend = f
        .state()
        .backend
        .take()
        .expect("the fixture has a renderer");
    let elements = crate::render::compose_frame(f.state(), backend.renderer(), output, Vec::new());
    f.state().backend = Some(backend);
    elements
}

/// Map an opaque window, park the view at `zoom` with the window centred, and
/// draw an age-0 frame followed by an idle age-1 frame. Returns the idle frame's
/// damage rects and whether the window's content was composed as a plain
/// `CsdWindow`: an animated one reports no opaque regions, so there would be no
/// band to re-damage and an empty result would prove nothing.
fn damage_of_two_frames(
    renderer: GlesRenderer,
    zoom: f64,
) -> (Vec<Rectangle<i32, Physical>>, bool) {
    let mut f = Fixture::with_config(super::config(CONFIG));
    let output = f.add_output(1, (OUTPUT.0 as u16, OUTPUT.1 as u16));
    gl::install(&mut f, renderer);

    let client = f.add_client();
    let surface = map_window(&mut f, client, "opaque", WINDOW);
    f.client(client)
        .set_opaque_region(&surface, &[(0, 0, WINDOW.0 as i32, WINDOW.1 as i32)]);
    // smithay rebuilds a surface's opaque regions only when a new buffer arrives.
    let window = f.client(client).window(&surface);
    window.attach_new_buffer();
    window.commit();
    f.roundtrip(client);
    super::tick_until_settled(&mut f);

    let window = window_by_app_id(&mut f, "opaque").expect("the window mapped");
    let loc = f.state().stage.position_of(&window).expect("on the stage");
    let centre = Point::<f64, Logical>::from((
        f64::from(loc.x) + f64::from(WINDOW.0) / 2.0,
        f64::from(loc.y) + f64::from(WINDOW.1) / 2.0,
    ));
    f.state().with_output_state(|os| {
        os.zoom = zoom;
        os.zoom_target = None;
        os.camera = Point::from((
            centre.x - f64::from(OUTPUT.0) / 2.0 / zoom,
            centre.y - f64::from(OUTPUT.1) / 2.0 / zoom,
        ));
        os.camera_target = None;
    });

    let elements = compose(&mut f, &output);
    let mut tracker = OutputDamageTracker::new(OUTPUT, 1.0, Transform::Normal);
    let mut rects = Vec::new();
    tracker.damage_output(0, &elements).unwrap();
    if let (Some(idle), _) = tracker.damage_output(1, &elements).unwrap() {
        rects.extend(idle.iter().copied());
    }
    let has_csd_window = elements
        .iter()
        .any(|element| matches!(element, crate::render::OutputRenderElements::CsdWindow(_)));

    drop(elements);
    let name = output.name();
    f.state().render.remove_output(&name);
    gl::uninstall(&mut f);
    (rects, has_csd_window)
}

#[test]
#[ignore = "needs Mesa surfaceless EGL; run with --include-ignored"]
fn an_idle_frame_has_no_damage_at_full_zoom() {
    let _gl = gl::lock();
    let Some(renderer) = gl::surfaceless_renderer("zoomed damage") else {
        return;
    };

    let (idle_damage, has_csd_window) = damage_of_two_frames(renderer, 1.0);

    assert!(has_csd_window, "no unanimated CsdWindow was composed");
    assert_eq!(idle_damage, vec![]);
}

#[test]
#[ignore = "needs Mesa surfaceless EGL; run with --include-ignored"]
fn an_idle_frame_has_no_damage_when_zoom_collapses_the_corner_bands() {
    let _gl = gl::lock();
    let Some(renderer) = gl::surfaceless_renderer("zoomed damage") else {
        return;
    };

    let (idle_damage, has_csd_window) = damage_of_two_frames(renderer, 0.1);

    assert!(has_csd_window, "no unanimated CsdWindow was composed");
    assert_eq!(idle_damage, vec![]);
}
