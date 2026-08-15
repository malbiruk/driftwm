//! A fullscreen window carrying a rule `opacity` below 1.0 is drawn see-through,
//! so the canvas behind it must keep rendering. `fullscreen_conceals_canvas` is
//! the predicate that decides it, and it is deliberately *narrower* than
//! `is_output_visually_fullscreen` rather than a widening of it: the layer
//! buckets, the other windows and the pinned ones keep culling on coverage, and
//! only the background, the canvas layers and the outlines answer to
//! concealment.
//!
//! Backend is `None`, so nothing here composes a frame. Every scenario asserts
//! the predicate the composer gates those three buckets on.

use smithay::desktop::Window;
use smithay::output::Output;
use smithay::utils::Point;

use crate::ipc::dispatch;
use crate::ipc::protocol::{Request, Response};

use super::client::ClientId;
use super::{Fixture, config, map_window, tick_until_settled, window_by_app_id};

/// A `[[window_rules]]` block seeding app_id `fs` with `opacity`, or no rule at
/// all when `opacity` is `None`.
fn config_with_opacity(opacity: Option<f64>) -> driftwm::config::Config {
    match opacity {
        Some(value) => config(&format!(
            "[[window_rules]]\napp_id = \"fs\"\nopacity = {value}\n"
        )),
        None => config(""),
    }
}

/// A window mapped at `size`, driven fullscreen by the client and settled, on a
/// fresh `1920x1080` output at zoom 1.
fn settled_fullscreen(
    f: &mut Fixture,
    size: (u16, u16),
) -> (
    ClientId,
    wayland_client::protocol::wl_surface::WlSurface,
    Output,
    Window,
) {
    let output = f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let surface = map_window(f, id, "fs", size);
    let window = window_by_app_id(f, "fs").unwrap();
    f.client(id).window(&surface).set_fullscreen(None);
    f.double_roundtrip(id);
    super::adopt_last_configure(f, id, &surface);
    tick_until_settled(f);
    (id, surface, output, window)
}

/// The headline: a rule opacity below 1.0 stops the fullscreen picture
/// concealing the canvas, while the output stays *covered*. Both halves matter —
/// the second is what catches a fix that widened the coverage predicate instead
/// of adding a narrower one beside it, which would pop the panels and every
/// other window back in over the fullscreen frame.
#[test]
fn a_translucent_fullscreen_window_does_not_conceal_the_canvas() {
    let mut f = Fixture::with_config(config_with_opacity(Some(0.5)));
    let (_id, _surface, output, _window) = settled_fullscreen(&mut f, (800, 600));

    assert!(
        f.state().is_output_visually_fullscreen(&output),
        "the picture still covers the output — only the canvas stops being culled"
    );
    assert!(
        !f.state().fullscreen_conceals_canvas(&output),
        "a translucent fullscreen window has to leave the canvas drawn behind it"
    );

    f.state().exit_fullscreen_on(&output);
}

/// The control, both ways an opaque window reads as opaque: an explicit
/// `opacity = 1.0` rule, and no rule at all (the `unwrap_or(1.0)` path).
#[test]
fn an_opaque_fullscreen_window_conceals_the_canvas() {
    for rule in [Some(1.0), None] {
        let mut f = Fixture::with_config(config_with_opacity(rule));
        let (_id, _surface, output, _window) = settled_fullscreen(&mut f, (800, 600));

        assert!(
            f.state().fullscreen_conceals_canvas(&output),
            "an opaque fullscreen window conceals the canvas (rule {rule:?})"
        );

        f.state().exit_fullscreen_on(&output);
    }
}

/// Under-fill is not a trigger. A client that answers the fullscreen configure
/// smaller than the output is centred in it and leaves a band of clear colour
/// around itself — but while it is opaque it still conceals, so that band stays
/// black rather than turning into a window-shaped hole onto the canvas. Inverse
/// of `a_smaller_fullscreen_commit_is_centred_and_still_reads_as_covering_the_output`:
/// fold under-fill into the predicate and this one flips.
#[test]
fn an_under_filling_fullscreen_window_still_conceals_the_canvas() {
    let mut f = Fixture::with_config(config_with_opacity(None));
    let output = f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let surface = map_window(&mut f, id, "fs", (700, 500));
    let window = window_by_app_id(&mut f, "fs").unwrap();
    f.state().enter_fullscreen(&window, Some(output.clone()));
    f.double_roundtrip(id);
    let camera = f.state().camera().to_i32_round();

    // Ack the fullscreen offer at a smaller size than the one configured, the
    // way a fixed-aspect-ratio client does.
    f.double_roundtrip(id);
    f.client(id).window(&surface).set_size(800, 600);
    f.client(id).window(&surface).attach_new_buffer();
    f.client(id).window(&surface).ack_last_and_commit();
    f.double_roundtrip(id);
    tick_until_settled(&mut f);

    let position = f.state().stage.position_of(&window).expect("staged");
    assert_eq!(
        position - camera,
        Point::from((560, 240)),
        "precondition: the window really under-fills, centred in the output"
    );
    assert!(
        f.state().fullscreen_conceals_canvas(&output),
        "under-fill alone must never uncover the canvas — the band around an \
         opaque window stays the clear colour"
    );

    f.state().exit_fullscreen_on(&output);
}

/// The runtime path: the `opacity` IPC verb writes the same applied rule the
/// predicate reads, so the answer changes on the spot — no animation tick, no
/// stage change, no re-entry into fullscreen.
#[test]
fn setting_opacity_over_ipc_stops_concealing_the_canvas() {
    let mut f = Fixture::with_config(config_with_opacity(None));
    let (_id, _surface, output, _window) = settled_fullscreen(&mut f, (800, 600));
    assert!(
        f.state().fullscreen_conceals_canvas(&output),
        "precondition: it starts opaque, so it conceals"
    );

    let set = dispatch(
        Request::Opacity {
            window: None,
            value: Some(0.5),
        },
        f.state(),
    );
    assert_eq!(set, Ok(Response::Opacity(0.5)));

    assert!(
        f.state().is_output_visually_fullscreen(&output),
        "the picture is unchanged — it still covers the output"
    );
    assert!(
        !f.state().fullscreen_conceals_canvas(&output),
        "but the canvas has to come back the moment the value is written"
    );

    f.state().exit_fullscreen_on(&output);
}

/// The transition half. A fullscreen exit lets go of stage membership at the
/// action but holds the fullscreen picture on screen for the length of its
/// freeze, and the rule has to hold across that: the output stays *covered* so
/// the panels do not pop back in, while the canvas it is see-through onto stays
/// drawn.
///
/// The coverage assertion is the one that fails if the freeze is ever taught to
/// stop claiming coverage for a translucent picture — a change that would
/// uncover every bucket, not just the canvas.
#[test]
fn a_frozen_exit_of_a_translucent_fullscreen_window_still_covers_but_conceals_nothing() {
    let mut f = Fixture::with_config(config_with_opacity(Some(0.5)));
    let (id, surface, output, window) = settled_fullscreen(&mut f, (800, 600));
    let eid = f.state().stage.id_of(&window).expect("staged");

    f.state().exit_fullscreen_on(&output);
    f.double_roundtrip(id);
    assert!(
        f.state().window_animations.start_held(eid),
        "precondition: the exit is frozen, waiting for the client's windowed redraw"
    );

    assert!(
        f.state().is_output_visually_fullscreen(&output),
        "the frozen picture has not moved, so the output stays covered"
    );
    assert_eq!(
        f.state().visually_fullscreen_windows_on(&output),
        vec![window.clone()],
        "and the window on its way out is the one drawing it"
    );
    assert!(
        !f.state().fullscreen_conceals_canvas(&output),
        "and it is still translucent, so the canvas stays drawn under it"
    );

    super::adopt_last_configure(&mut f, id, &surface);
    tick_until_settled(&mut f);
}

/// The animated background has to keep ticking on an output showing its canvas
/// through a translucent fullscreen window: this filter gates the idle
/// due-check, the udev tick-timer arming and the per-frame dirty marking alike,
/// so dropping the output here draws the wallpaper frozen rather than not at
/// all. Turning the opacity back up drops it out again, so eligibility really
/// tracks the rule rather than having stopped filtering at all.
#[test]
fn a_translucent_fullscreen_output_stays_background_render_eligible() {
    let mut f = Fixture::with_config(config_with_opacity(Some(0.5)));
    let (_id, _surface, output, _window) = settled_fullscreen(&mut f, (800, 600));
    // `active_outputs` is the udev backend's set; the fixture has no backend, so
    // seat it by hand or the filter has nothing to filter.
    f.state().active_outputs.insert(output.clone());

    assert!(
        f.state()
            .background_render_eligible_outputs()
            .any(|o| *o == output),
        "the output still renders its background while the window is see-through"
    );

    let set = dispatch(
        Request::Opacity {
            window: None,
            value: Some(1.0),
        },
        f.state(),
    );
    assert_eq!(set, Ok(Response::Opacity(1.0)));
    assert!(
        !f.state()
            .background_render_eligible_outputs()
            .any(|o| *o == output),
        "and drops back out of the set the moment the window turns opaque"
    );

    f.state().exit_fullscreen_on(&output);
}
