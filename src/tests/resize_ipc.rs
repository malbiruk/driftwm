//! Non-interactive resize over the IPC `resize` verb: a read reports the
//! committed size, a set configures a clamped size and re-places the window
//! around its own visual center, and a stand-in resizes in place with no client
//! to configure at all.

use smithay::desktop::Window;
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
use smithay::reexports::wayland_server::Resource;
use smithay::utils::{Logical, Point, Size};
use wayland_client::protocol::wl_surface::WlSurface as ClientSurface;

use super::client::ClientId;
use super::real::TempDir;
use super::{
    Fixture, ack_but_keep_size, adopt_last_configure, client_sees_maximized, config,
    configure_count, end_grab, install_client_resize_grab, last_configured, map_window,
    map_window_with_limits, seed_fit_and_fill, server_surface, window_by_app_id, window_position,
};
use crate::ipc::dispatch;
use crate::ipc::protocol::{Reply, Request, Response, WindowSelector};
use crate::state::{ClusterResizeSnapshot, StageWindow, SuspendedId};
use driftwm::config::{Action, Direction};

fn resize(f: &mut Fixture, window: Option<WindowSelector>, to: Option<(i32, i32)>) -> Reply {
    dispatch(Request::Resize { window, to }, f.state())
}

/// Visual center from *committed* geometry. `window_visual_center` sizes itself
/// from the last configure, which is the very thing a drift check must not
/// assume the client honoured.
fn committed_center(f: &mut Fixture, window: &Window) -> Point<f64, Logical> {
    let loc = window_position(f, window);
    let bar = f.state().window_ssd_bar(window) as f64;
    crate::state::visual_frame_center(loc, window.geometry().size, bar)
}

/// The cached snap rect's dimensions minus the chrome the cache inflates by —
/// the content size `stable_snap_rects` believes the window has.
fn cached_content_size(f: &mut Fixture, window: &Window) -> (f64, f64) {
    let surface = server_surface(window);
    let bar = f.state().window_ssd_bar(window) as f64;
    let bw = f.state().window_border_width(&surface) as f64;
    let rect = f.state().stable_snap_rects[&surface.id()];
    (
        rect.x_high - rect.x_low - 2.0 * bw,
        rect.y_high - rect.y_low - bar - 2.0 * bw,
    )
}

/// Commit a size the compositor never asked for, as a client does on a font
/// change or its own `resize()` call. Nothing to ack — no configure is owed.
fn client_resizes_itself(f: &mut Fixture, id: ClientId, surface: &ClientSurface, size: (u16, u16)) {
    let window = f.client(id).window(surface);
    window.set_size(size.0, size.1);
    window.attach_new_buffer();
    window.commit();
    f.double_roundtrip(id);
}

fn stand_in_element(f: &mut Fixture, sid: SuspendedId) -> StageWindow {
    StageWindow::Suspended(f.state().find_suspended(sid).expect("stand-in"))
}

#[test]
fn read_returns_the_committed_size() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    map_window(&mut f, id, "term", (400, 300));

    assert_eq!(
        resize(&mut f, None, None),
        Ok(Response::Size {
            width: 400,
            height: 300
        })
    );
}

#[test]
fn a_set_configures_the_request_and_holds_the_center() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let surface = map_window(&mut f, id, "term", (400, 300));
    let window = window_by_app_id(&mut f, "term").unwrap();

    let before = window_position(&mut f, &window);
    let center = committed_center(&mut f, &window);

    assert_eq!(
        resize(&mut f, None, Some((600, 500))),
        Ok(Response::Size {
            width: 600,
            height: 500
        })
    );
    assert_eq!(last_configured(&mut f, id, &surface), (600, 500));
    assert_eq!(
        window_position(&mut f, &window),
        before + Point::from((-100, -100)),
        "the window shifted by half the size delta on each axis"
    );
    // See `resize_element_to`'s snap-rect note: cached from the request, not
    // pre-ack geometry, or a later commit would read as "grew past settled".
    assert_eq!(cached_content_size(&mut f, &window), (600.0, 500.0));

    adopt_last_configure(&mut f, id, &surface);
    assert_eq!(window.geometry().size, Size::from((600, 500)));
    assert_eq!(
        committed_center(&mut f, &window),
        center,
        "the acked size lands on the center the window started from"
    );
    assert_eq!(
        window_position(&mut f, &window),
        before + Point::from((-100, -100)),
        "and the commit left the placement alone — no reflow relocation"
    );
}

#[test]
fn a_request_outside_the_clients_limits_is_clamped() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let surface = map_window_with_limits(&mut f, id, "term", (800, 600), (500, 400), (1000, 900));

    assert_eq!(
        resize(&mut f, None, Some((200, 100))),
        Ok(Response::Size {
            width: 500,
            height: 400
        }),
        "a script cannot know a client's declared minimum, so the request clamps"
    );
    assert_eq!(last_configured(&mut f, id, &surface), (500, 400));

    assert_eq!(
        resize(&mut f, None, Some((4000, 3000))),
        Ok(Response::Size {
            width: 1000,
            height: 900
        }),
        "and the declared maximum is the other bound"
    );
    assert_eq!(last_configured(&mut f, id, &surface), (1000, 900));
}

#[test]
fn a_repeat_request_before_the_ack_does_not_walk_the_window() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let surface = map_window(&mut f, id, "term", (400, 300));
    let window = window_by_app_id(&mut f, "term").unwrap();
    let before = window_position(&mut f, &window);

    resize(&mut f, None, Some((600, 500))).unwrap();
    let after_first = window_position(&mut f, &window);
    assert_eq!(after_first, before + Point::from((-100, -100)));
    let configures = configure_count(&mut f, id, &surface);

    // The client has not acked, so committed geometry is still 400x300. Measured
    // against that, the second request would shift the window by another half
    // delta; measured against the size already configured it is a no-op.
    resize(&mut f, None, Some((600, 500))).unwrap();
    assert_eq!(window_position(&mut f, &window), after_first);
    assert_eq!(
        configure_count(&mut f, id, &surface),
        configures,
        "a zero-delta request sends no configure at all"
    );
}

/// The ack is not the commit: smithay drops the pending configure the moment the
/// client acks, and real clients ack as soon as they process the event and go on
/// committing their old size. Measured against committed geometry from there, a
/// re-run of the same layout script re-derives from the pre-resize size and
/// shifts the window another half-delta every call — against a fixed-size dialog,
/// forever, with no size change to show for it.
#[test]
fn a_repeat_request_after_the_ack_does_not_walk_the_window() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let surface = map_window(&mut f, id, "term", (400, 300));
    let window = window_by_app_id(&mut f, "term").unwrap();

    resize(&mut f, None, Some((600, 500))).unwrap();
    let after_first = window_position(&mut f, &window);
    ack_but_keep_size(&mut f, id, &surface);
    assert_eq!(
        window.geometry().size,
        Size::from((400, 300)),
        "precondition: acked, and still committing the size it already had"
    );
    let configures = configure_count(&mut f, id, &surface);

    for _ in 0..3 {
        resize(&mut f, None, Some((600, 500))).unwrap();
    }

    assert_eq!(window_position(&mut f, &window), after_first);
    assert_eq!(
        configure_count(&mut f, id, &surface),
        configures,
        "the request already outstanding makes each repeat a no-op"
    );
}

/// An outstanding request only speaks for the window until something else
/// configures a size of its own. A fit in between makes the fit's size the live
/// one, so a request for the size that was outstanding before it has to reach
/// the client instead of being swallowed as a repeat.
#[test]
fn a_fit_between_two_identical_requests_does_not_swallow_the_second() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let surface = map_window(&mut f, id, "term", (400, 300));
    let window = window_by_app_id(&mut f, "term").unwrap();

    resize(&mut f, None, Some((600, 500))).unwrap();
    f.double_roundtrip(id);
    adopt_last_configure(&mut f, id, &surface);
    f.state().fit_window(&window);
    f.double_roundtrip(id);
    let configures = configure_count(&mut f, id, &surface);

    resize(&mut f, None, Some((600, 500))).unwrap();

    assert_eq!(last_configured(&mut f, id, &surface), (600, 500));
    assert_eq!(configure_count(&mut f, id, &surface), configures + 1);
}

/// A step that finds nothing to do must not disarm the request an absolute
/// resize left outstanding: dropping the record early puts the window straight
/// back on the walk it exists to prevent.
#[test]
fn a_no_op_step_between_two_requests_does_not_rearm_the_walk() {
    let mut f = Fixture::with_config(config(
        r#"
[navigation]
resize_step = 0
"#,
    ));
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let surface = map_window(&mut f, id, "term", (400, 300));
    let window = window_by_app_id(&mut f, "term").unwrap();

    resize(&mut f, None, Some((600, 500))).unwrap();
    ack_but_keep_size(&mut f, id, &surface);
    let after_first = window_position(&mut f, &window);
    let configures = configure_count(&mut f, id, &surface);

    f.state()
        .execute_action(&Action::GrowWindow(Direction::Right));
    resize(&mut f, None, Some((600, 500))).unwrap();

    assert_eq!(window_position(&mut f, &window), after_first);
    assert_eq!(configure_count(&mut f, id, &surface), configures);
}

/// The compositor cannot tell a client that answered by rounding the size it was
/// handed from one that resized itself later to a size in the same range: both
/// are a commit between what the window had and what it was asked for. Reading
/// that as an answer is what keeps a repeated request from walking a
/// cell-snapping terminal, so the rarer case pays for it — an in-band self-resize
/// is misread, and the identical request that follows is a no-op. A request for
/// any other size still reaches the client.
#[test]
fn an_in_band_self_resize_is_misread_as_an_answer() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let surface = map_window(&mut f, id, "term", (400, 300));
    let window = window_by_app_id(&mut f, "term").unwrap();

    resize(&mut f, None, Some((800, 600))).unwrap();
    f.double_roundtrip(id);
    adopt_last_configure(&mut f, id, &surface);
    client_resizes_itself(&mut f, id, &surface, (600, 450));
    let configures = configure_count(&mut f, id, &surface);
    let before = window_position(&mut f, &window);

    assert_eq!(
        resize(&mut f, None, Some((800, 600))),
        Ok(Response::Size {
            width: 800,
            height: 600
        })
    );
    assert_eq!(
        configure_count(&mut f, id, &surface),
        configures,
        "the repeat is swallowed: 600x450 still reads as an answer to 800x600"
    );

    assert!(resize(&mut f, None, Some((810, 600))).is_ok());
    assert_eq!(
        last_configured(&mut f, id, &surface),
        (810, 600),
        "any other size is measured from the outstanding request and reaches the client"
    );
    assert_eq!(
        window_position(&mut f, &window),
        before + Point::from((-5, 0)),
        "and is anchored on the rect that request placed"
    );
}

/// Mirrors `requested_element_size`'s own note: once a client resizes itself,
/// the last configure is stale, and a request must measure from committed
/// geometry instead.
#[test]
fn a_client_that_resized_itself_is_measured_from_committed_geometry() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let surface = map_window(&mut f, id, "term", (400, 300));
    let window = window_by_app_id(&mut f, "term").unwrap();

    resize(&mut f, None, Some((600, 500))).unwrap();
    f.double_roundtrip(id);
    adopt_last_configure(&mut f, id, &surface);
    client_resizes_itself(&mut f, id, &surface, (700, 600));
    assert_eq!(window.geometry().size, Size::from((700, 600)));

    let before = window_position(&mut f, &window);
    let configures = configure_count(&mut f, id, &surface);

    resize(&mut f, None, Some((600, 500))).unwrap();

    assert_eq!(
        last_configured(&mut f, id, &surface),
        (600, 500),
        "the request reached the client instead of being swallowed as a no-op"
    );
    assert_eq!(configure_count(&mut f, id, &surface), configures + 1);
    assert_eq!(
        window_position(&mut f, &window),
        before + Point::from((50, 50)),
        "and the anchor came off the 700x600 the window actually had"
    );
}

#[test]
fn a_client_that_snaps_its_size_drifts_at_most_half_the_snap() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let surface = map_window(&mut f, id, "term", (400, 300));
    let window = window_by_app_id(&mut f, "term").unwrap();
    let center = committed_center(&mut f, &window);

    resize(&mut f, None, Some((603, 503))).unwrap();

    // A cell-snapping terminal commits its own nearest size rather than the one
    // it was handed; the placement was already written from the request.
    let client_window = f.client(id).window(&surface);
    client_window.set_size(600, 500);
    client_window.attach_new_buffer();
    client_window.ack_last_and_commit();
    f.double_roundtrip(id);
    assert_eq!(window.geometry().size, Size::from((600, 500)));

    let drift = committed_center(&mut f, &window) - center;
    assert!(
        drift.x.abs() <= 1.0 && drift.y.abs() <= 1.0,
        "the center drift stays within half the snap error, got {drift:?}"
    );
}

#[test]
fn non_positive_dimensions_are_rejected() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    map_window(&mut f, id, "term", (400, 300));
    let window = window_by_app_id(&mut f, "term").unwrap();
    let before = window_position(&mut f, &window);

    for bad in [(0, 300), (400, 0), (-100, 300)] {
        assert_eq!(
            resize(&mut f, None, Some(bad)),
            Err("size must be positive".to_string()),
            "{bad:?} must be rejected"
        );
    }
    assert_eq!(window_position(&mut f, &window), before);
    assert_eq!(window.geometry().size, Size::from((400, 300)));
}

#[test]
fn pinned_and_widget_windows_are_refused() {
    let mut f = Fixture::with_config(config(
        r#"
[[window_rules]]
app_id = "pin"
pinned_to_screen = true
size = [320, 240]

[[window_rules]]
app_id = "bar"
widget = true
"#,
    ));
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    map_window(&mut f, id, "pin", (320, 240));
    map_window(&mut f, id, "bar", (200, 100));

    for app_id in ["pin", "bar"] {
        let window = window_by_app_id(&mut f, app_id).unwrap();
        let window_id = f.state().stage.id_of(&window).unwrap().0;
        let reply = resize(
            &mut f,
            Some(WindowSelector::Id(window_id)),
            Some((640, 480)),
        );
        assert!(
            reply
                .as_ref()
                .is_err_and(|e| e.contains("no canvas size to set")),
            "{app_id} holds no canvas rect a resize could write, got {reply:?}"
        );
    }
}

#[test]
fn a_fullscreen_window_is_refused() {
    let mut f = Fixture::new();
    let output = f.add_output(1, (1920, 1080));
    let id = f.add_client();
    map_window(&mut f, id, "term", (400, 300));
    let window = window_by_app_id(&mut f, "term").unwrap();

    f.state().enter_fullscreen(&window, Some(output.clone()));
    let reply = resize(&mut f, None, Some((640, 480)));
    assert!(
        reply
            .as_ref()
            .is_err_and(|e| e.contains("no canvas size to set")),
        "got {reply:?}"
    );

    f.state().exit_fullscreen_on(&output);
}

#[test]
fn a_set_clears_fit_and_fill_membership() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let surface = map_window(&mut f, id, "term", (400, 300));
    let window = window_by_app_id(&mut f, "term").unwrap();
    seed_fit_and_fill(&mut f, &window);

    resize(&mut f, None, Some((600, 500))).unwrap();

    assert_eq!(f.state().stage.fit_saved_size(&window), None);
    assert!(!f.state().stage.is_fill(&window));
    f.double_roundtrip(id);
    assert!(
        !client_sees_maximized(&mut f, id, &surface),
        "the fit clear was mirrored to the client, or its restore button is dead"
    );
}

/// A resize is the window's new placement, so it drops the recenter a fullscreen
/// exit still owes — which would otherwise fire on the very commit this
/// configure provokes, and undo it.
#[test]
fn a_set_drops_the_windows_owed_recenter() {
    let mut f = Fixture::new();
    let output = f.add_output(1, (1920, 1080));
    f.skip_baseline_check();
    let id = f.add_client();
    let surface = map_window(&mut f, id, "term", (400, 300));
    let window = window_by_app_id(&mut f, "term").unwrap();
    let key = server_surface(&window).id();

    // The exit only owes a recenter while the client is still committing the
    // fullscreen size, so adopt that size before exiting.
    f.state().enter_fullscreen(&window, Some(output.clone()));
    f.double_roundtrip(id);
    adopt_last_configure(&mut f, id, &surface);
    f.state().exit_fullscreen_on(&output);
    assert!(
        f.state().pending_recenter.contains_key(&key),
        "precondition: the exit owes a recenter"
    );

    resize(&mut f, None, Some((600, 500))).unwrap();

    assert!(!f.state().pending_recenter.contains_key(&key));
}

#[test]
fn the_new_size_survives_a_fullscreen_round_trip() {
    let mut f = Fixture::new();
    let output = f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let surface = map_window(&mut f, id, "term", (400, 300));
    let window = window_by_app_id(&mut f, "term").unwrap();

    // Deliberately no ack: the restore point has to be recorded from the size
    // that was requested, since no later commit is obliged to confirm it.
    resize(&mut f, None, Some((600, 500))).unwrap();

    f.state().enter_fullscreen(&window, Some(output.clone()));
    f.state().exit_fullscreen_on(&output);

    assert_eq!(
        last_configured(&mut f, id, &surface),
        (600, 500),
        "the fullscreen exit restores the resized rect, not the pre-resize one"
    );
}

#[test]
fn a_set_is_refused_under_a_move_grab() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    map_window(&mut f, id, "term", (400, 300));
    let window = window_by_app_id(&mut f, "term").unwrap();
    let before = window_position(&mut f, &window);

    f.state().arm_interactive_move(&window);
    let reply = resize(&mut f, None, Some((600, 500)));
    assert!(
        reply
            .as_ref()
            .is_err_and(|e| e.contains("interactive move or resize")),
        "a live grab recomputes the rect every motion tick, got {reply:?}"
    );
    assert_eq!(window_position(&mut f, &window), before);

    f.state().disarm_interactive_move(&window);
    assert!(resize(&mut f, None, Some((600, 500))).is_ok());
}

/// The client-resize half is a separate witness: a `ResizeGrab` over a client
/// arms no `interactive_move` entry, only the surface's `ResizeState`. Guarding
/// on grab membership alone would let this one through.
#[test]
fn a_set_is_refused_under_a_client_resize_grab() {
    let mut f = Fixture::new();
    let output = f.add_output(1, (1920, 1080));
    let id = f.add_client();
    map_window(&mut f, id, "term", (400, 300));
    let window = window_by_app_id(&mut f, "term").unwrap();
    let before = window_position(&mut f, &window);

    install_client_resize_grab(
        &mut f,
        &window,
        xdg_toplevel::ResizeEdge::Right,
        Point::from((before.x as f64 + 390.0, before.y as f64 + 150.0)),
        output,
        ClusterResizeSnapshot::empty(),
    );

    let reply = resize(&mut f, None, Some((600, 500)));
    assert!(
        reply
            .as_ref()
            .is_err_and(|e| e.contains("interactive move or resize")),
        "got {reply:?}"
    );
    assert_eq!(window_position(&mut f, &window), before);

    end_grab(&mut f);
}

#[test]
fn a_stand_in_resize_clamps_marks_the_store_and_bumps_blur() {
    let tmp = TempDir::new();
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    f.state().session_store.path = Some(tmp.path().join("session.json"));

    let sid = f.state().insert_suspended_for_test(
        1,
        Point::from((400, 300)),
        Size::from((400, 300)),
        "s",
        "S",
    );
    let element = stand_in_element(&mut f, sid);
    let window_id = f.state().stage.id_of(&element).unwrap().0;
    let generation = f.state().render.blur_geometry_generation;

    assert_eq!(
        resize(&mut f, Some(WindowSelector::Id(window_id)), Some((80, 90))),
        Ok(Response::Size {
            width: 120,
            height: 120
        }),
        "a stand-in has no client to declare a minimum, so the chrome floor applies"
    );
    assert_eq!(
        f.state().find_suspended(sid).unwrap().size.get(),
        Size::from((120, 120))
    );
    assert_eq!(
        f.state().stage.position_of(&element),
        Some(Point::from((540, 390))),
        "the stand-in keeps its center"
    );
    assert!(
        f.state().render.blur_geometry_generation > generation,
        "nothing else bumps the blur generation for a stand-in — no commit follows"
    );
    assert!(
        f.state().session_store_dirty(),
        "the stand-in's new size is queued for the durable write"
    );
    assert_eq!(
        f.state().stage.restore_size(&element),
        None,
        "a stand-in entry carries no restore size: `Stage::replace` would hand it \
         to the window adopted into this slot and drop the real adopt size"
    );

    // Cancels the debounce timer the mark armed; `debug_counters` has no entry
    // for event-loop timers, so the teardown baseline would not catch one.
    f.state().dismiss_suspended(sid);
}

/// Resizing a stand-in leaves the z-order alone, exactly as `msg move` does — an
/// IPC call aimed at an unfocused stand-in must not pull it over the window on
/// top of it.
#[test]
fn a_stand_in_resize_does_not_raise_it() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let sid = f.state().insert_suspended_for_test(
        1,
        Point::from((400, 300)),
        Size::from((400, 300)),
        "s",
        "S",
    );
    let element = stand_in_element(&mut f, sid);
    let window_id = f.state().stage.id_of(&element).unwrap().0;

    let id = f.add_client();
    map_window(&mut f, id, "top", (400, 300));
    let top = window_by_app_id(&mut f, "top").unwrap();
    assert_eq!(
        f.state().stage.windows().last().and_then(|w| w.client()),
        Some(&top),
        "precondition: the client sits above the stand-in"
    );

    resize(
        &mut f,
        Some(WindowSelector::Id(window_id)),
        Some((200, 200)),
    )
    .unwrap();

    assert_eq!(
        f.state().stage.windows().last().and_then(|w| w.client()),
        Some(&top),
        "the stand-in resized in place without re-raising"
    );

    f.state().dismiss_suspended(sid);
}

#[test]
fn a_stand_in_read_reports_its_own_size() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let sid = f.state().insert_suspended_for_test(
        1,
        Point::from((400, 300)),
        Size::from((400, 300)),
        "s",
        "S",
    );
    let element = stand_in_element(&mut f, sid);
    let window_id = f.state().stage.id_of(&element).unwrap().0;

    assert_eq!(
        resize(&mut f, Some(WindowSelector::Id(window_id)), None),
        Ok(Response::Size {
            width: 400,
            height: 300
        })
    );

    f.state().dismiss_suspended(sid);
}
