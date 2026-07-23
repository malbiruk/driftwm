//! Exact configure sequences as the client sees them — the desync class where
//! a toolkit acks one configure while the compositor already believes another.

use driftwm::config::{Action, Config, DecorationMode};
use smithay::reexports::wayland_server::Resource;
use smithay::utils::{Point, Size};

use crate::state::StageWindow;

use super::{Fixture, adopt_last_configure, window_by_app_id};

/// Map one toplevel with a buffer at `size`, settle, and drain the configure
/// cursor so tests only see what happens next.
fn map_settled(
    f: &mut Fixture,
    id: super::client::ClientId,
    app_id: &str,
    size: (u16, u16),
) -> wayland_client::protocol::wl_surface::WlSurface {
    let window = f.client(id).create_window();
    let surface = window.surface.clone();
    window.set_app_id(app_id);
    window.commit();
    f.roundtrip(id);

    let window = f.client(id).window(&surface);
    window.set_size(size.0, size.1);
    window.attach_new_buffer();
    window.ack_last_and_commit();
    f.double_roundtrip(id);
    f.client(id).window(&surface).format_recent_configures();
    surface
}

/// A window snapped only to a suspended stand-in reflows when it grows into it,
/// exactly as it would beside a live window: the grow-reflow neighbor set counts
/// stand-ins, so a font-bump next to a stand-in relocates instead of overlapping.
#[test]
fn grow_reflows_off_a_stand_in_neighbor() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    // A settled live window "a"; pin the camera (mapping pans it) and park it.
    let a_surface = map_settled(&mut f, id, "a", (800, 600));
    let a = window_by_app_id(&mut f, "a").unwrap();
    let a_elem = StageWindow::Client(a.clone());
    f.state().set_camera(Point::from((0.0, 0.0)));
    f.state()
        .map_window(a.clone(), Point::from((400, 300)), false);
    f.state().refresh_stable_snap_rect(&a_elem);

    // A stand-in as "a"'s only neighbor, gap-adjacent to its right edge and
    // y-overlapping (the reflow's "was snapped" anchor precondition).
    let a_frame = f.state().visual_frame_rect(&a_elem).unwrap();
    let gap = f.state().config.snap_gap as i32;
    let bw = f.state().default_border_width();
    let sx = a_frame.x_high as i32 + gap + bw;
    let sid = f.state().insert_suspended_for_test(
        1,
        Point::from((sx, 300)),
        Size::from((300, 400)),
        "s",
        "S",
    );
    let standin = StageWindow::Suspended(f.state().find_suspended(sid).unwrap());
    let standin_frame = f.state().visual_frame_rect(&standin).unwrap();

    let before = f.state().stage.position_of(&a_elem).unwrap();

    // "a" grows past the stand-in (a font bump would do this): a spontaneous
    // CSD resize larger than its settled width, colliding with the neighbor.
    let win = f.client(id).window(&a_surface);
    win.set_size(1000, 600);
    win.attach_new_buffer();
    win.commit();
    f.double_roundtrip(id);

    // The grow reflowed "a" off the stand-in instead of overlapping it.
    let after = f.state().stage.position_of(&a_elem).unwrap();
    assert_ne!(after, before, "the grown window relocated off the stand-in");
    let a_after = f.state().visual_frame_rect(&a_elem).unwrap();
    let overlaps = a_after.x_low < standin_frame.x_high
        && standin_frame.x_low < a_after.x_high
        && a_after.y_low < standin_frame.y_high
        && standin_frame.y_low < a_after.y_high;
    assert!(!overlaps, "and no longer overlaps the stand-in frame");

    f.state().dismiss_suspended(sid);
}

#[test]
fn initial_burst_is_a_single_configure() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    let window = f.client(id).create_window();
    let surface = window.surface.clone();
    window.commit();
    f.double_roundtrip(id);

    // Exactly one configure before the first ack — a second uncommitted
    // configure in the initial burst is what desyncs size-tracking toolkits.
    let window = f.client(id).window(&surface);
    assert_eq!(window.recent_configures().count(), 1);
}

#[test]
fn fullscreen_reassert_is_idempotent() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    let surface = map_settled(&mut f, id, "fs", (800, 600));
    let window = f.client(id).window(&surface);
    window.set_fullscreen(None);
    f.double_roundtrip(id);
    f.client(id).window(&surface).ack_last_and_commit();
    f.double_roundtrip(id);
    f.client(id).window(&surface).format_recent_configures();

    // Toolkits re-assert fullscreen on focus changes; the answer must be the
    // same fullscreen configure again, never an exit/re-enter bounce.
    let window = f.client(id).window(&surface);
    window.set_fullscreen(None);
    f.double_roundtrip(id);

    let window = f.client(id).window(&surface);
    let configures = window.format_recent_configures();
    for line in configures.lines() {
        assert!(
            line.contains("size: 1920 × 1080") && line.contains("Fullscreen"),
            "re-assert must only repeat the fullscreen configure, got:\n{configures}"
        );
    }
}

#[test]
fn second_fullscreen_displaces_first() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    let first = map_settled(&mut f, id, "first", (800, 600));
    let second = map_settled(&mut f, id, "second", (400, 300));

    let window = f.client(id).window(&first);
    window.set_fullscreen(None);
    f.double_roundtrip(id);
    f.client(id).window(&first).ack_last_and_commit();
    f.double_roundtrip(id);
    f.client(id).window(&first).format_recent_configures();

    let window = f.client(id).window(&second);
    window.set_fullscreen(None);
    f.double_roundtrip(id);

    // The displaced window is restored to its pre-fullscreen size...
    let first_configures = f.client(id).window(&first).format_recent_configures();
    assert!(
        first_configures.contains("size: 800 × 600") && !first_configures.contains("Fullscreen"),
        "displaced window must get its windowed configure back, got:\n{first_configures}"
    );
    // ...in a single configure whose strip rides the exit — no stale Activated
    // on it, and no separate back-to-back deactivate configure trailing it.
    assert_eq!(
        first_configures.lines().count(),
        1,
        "displaced window must get exactly one configure, got:\n{first_configures}"
    );
    assert!(
        !first_configures.contains("Activated"),
        "displaced window's exit configure must carry the deactivate, got:\n{first_configures}"
    );
    // ...and the new one owns the output.
    let second_configures = f.client(id).window(&second).format_recent_configures();
    assert!(
        second_configures.contains("size: 1920 × 1080") && second_configures.contains("Fullscreen"),
        "takeover window must get the fullscreen configure, got:\n{second_configures}"
    );
    let mapped = window_by_app_id(&mut f, "second").unwrap();
    assert_eq!(
        f.state().stage.fullscreen_output_of(&mapped),
        Some("HEADLESS-1")
    );
}

#[test]
fn fit_round_trip_restores_exact_size() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    // Even size on purpose: odd fitted sizes hit a known pre-existing 1px
    // truncation quirk that is not under test here.
    let surface = map_settled(&mut f, id, "fit", (800, 600));
    let window = window_by_app_id(&mut f, "fit").unwrap();

    f.state().toggle_fit_window(&window);
    f.double_roundtrip(id);
    let client_window = f.client(id).window(&surface);
    let fit_configures = client_window.format_recent_configures();
    assert!(
        fit_configures.contains("size:") && !fit_configures.contains("size: 800 × 600"),
        "fit must configure a new (viewport-fitted) size, got:\n{fit_configures}"
    );
    // Commit at the fitted size so the exit path restores from a fit-sized
    // window, as a real client would.
    let (w, h) = client_window.configures_received.last().unwrap().1.size;
    let client_window = f.client(id).window(&surface);
    client_window.set_size(w as u16, h as u16);
    client_window.ack_last_and_commit();
    f.double_roundtrip(id);

    f.state().toggle_fit_window(&window);
    f.double_roundtrip(id);
    let configures = f.client(id).window(&surface).format_recent_configures();
    assert!(
        configures.contains("size: 800 × 600"),
        "fit exit must restore the exact pre-fit size, got:\n{configures}"
    );
    assert!(!f.state().stage.is_fit(&window));
}

#[test]
fn fill_grows_to_usable_minus_gap() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    // Even size to sidestep the known 1px truncation quirk.
    let surface = map_settled(&mut f, id, "fill", (800, 600));
    let window = window_by_app_id(&mut f, "fill").unwrap();

    f.state().toggle_fill_window(&window);
    f.double_roundtrip(id);

    // Usable 1920×1080 minus a 12px gap on every side, no SSD bar / border on a
    // default CSD window → the content fills 1896×1056.
    let configures = f.client(id).window(&surface).format_recent_configures();
    assert!(
        configures.contains("size: 1896 × 1056"),
        "fill must configure the free-space size, got:\n{configures}"
    );
    assert!(f.state().stage.is_fill(&window));
}

#[test]
fn fill_round_trip_restores_size_and_position() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    let surface = map_settled(&mut f, id, "fill", (800, 600));
    let window = window_by_app_id(&mut f, "fill").unwrap();
    let pre_pos = f.state().stage.position_of(&window).unwrap();
    let pre_size = window.geometry().size;

    // Fill, then let the client adopt the filled size as a real client would.
    f.state().toggle_fill_window(&window);
    f.double_roundtrip(id);
    let cw = f.client(id).window(&surface);
    let (w, h) = cw.configures_received.last().unwrap().1.size;
    cw.set_size(w as u16, h as u16);
    cw.ack_last_and_commit();
    f.double_roundtrip(id);
    f.client(id).window(&surface).format_recent_configures();
    assert!(f.state().stage.is_fill(&window));

    // Unfill: the exit configure restores the exact pre-fill size, and once the
    // client commits it the pending recenter restores the pre-fill position.
    f.state().toggle_fill_window(&window);
    f.double_roundtrip(id);
    let configures = f.client(id).window(&surface).format_recent_configures();
    assert!(
        configures.contains(&format!("size: {} × {}", pre_size.w, pre_size.h)),
        "unfill must restore the exact pre-fill size, got:\n{configures}"
    );
    let cw = f.client(id).window(&surface);
    let (w, h) = cw.configures_received.last().unwrap().1.size;
    cw.set_size(w as u16, h as u16);
    cw.ack_last_and_commit();
    f.double_roundtrip(id);

    assert!(!f.state().stage.is_fill(&window));
    assert_eq!(
        f.state().stage.position_of(&window),
        Some(pre_pos),
        "unfill must restore the exact pre-fill position"
    );
}

#[test]
fn fill_stops_at_neighbor() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    let a_surface = map_settled(&mut f, id, "a", (800, 600));
    let _b_surface = map_settled(&mut f, id, "b", (400, 1056));
    let a = window_by_app_id(&mut f, "a").unwrap();
    let b = window_by_app_id(&mut f, "b").unwrap();

    // Park B to A's right, spanning the usable height, so it caps A's rightward
    // growth regardless of the axis order fill picks.
    f.state()
        .map_window(b.clone(), Point::from((500, -528)), false);
    let b_loc = f.state().stage.position_of(&b).unwrap();

    f.state().toggle_fill_window(&a);
    f.double_roundtrip(id);

    let gap = f.state().config.snap_gap as i32;
    let a_loc = f.state().stage.position_of(&a).unwrap();
    let (w, _h) = f
        .client(id)
        .window(&a_surface)
        .configures_received
        .last()
        .unwrap()
        .1
        .size;
    // A's right content edge stops exactly a gap short of B's left edge.
    assert_eq!(a_loc.x + w, b_loc.x - gap);
    assert!(f.state().stage.is_fill(&a));
}

#[test]
fn fill_shrinks_out_of_overlap_with_neighbor() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    let a_surface = map_settled(&mut f, id, "a", (800, 600));
    let _b_surface = map_settled(&mut f, id, "b", (400, 1056));
    let a = window_by_app_id(&mut f, "a").unwrap();
    let b = window_by_app_id(&mut f, "b").unwrap();

    // Park B spanning the usable height, then drop A so it overlaps B's left
    // portion. Fill must pull A's right edge back out of B before growing the
    // free sides — the shrink phase, not just growth stopping short.
    f.state()
        .map_window(b.clone(), Point::from((500, -528)), false);
    f.state()
        .map_window(a.clone(), Point::from((300, 0)), false);
    let b_loc = f.state().stage.position_of(&b).unwrap();

    f.state().toggle_fill_window(&a);
    f.double_roundtrip(id);

    let gap = f.state().config.snap_gap as i32;
    let a_loc = f.state().stage.position_of(&a).unwrap();
    let (w, _h) = f
        .client(id)
        .window(&a_surface)
        .configures_received
        .last()
        .unwrap()
        .1
        .size;
    // A's right content edge ends exactly a gap short of B's left edge, even
    // though A started overlapping B.
    assert_eq!(a_loc.x + w, b_loc.x - gap);
    assert!(f.state().stage.is_fill(&a));
}

#[test]
fn fill_on_fit_window_is_noop() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    let _surface = map_settled(&mut f, id, "fit", (800, 600));
    let window = window_by_app_id(&mut f, "fit").unwrap();

    f.state().toggle_fit_window(&window);
    assert!(f.state().stage.is_fit(&window));

    // A maximized-by-fit window is fit's business; fill leaves it untouched.
    f.state().fill_window(&window);
    assert!(!f.state().stage.is_fill(&window));
    assert!(f.state().stage.is_fit(&window));
}

#[test]
fn fill_on_pinned_window_is_noop() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    let _surface = map_settled(&mut f, id, "pin", (800, 600));
    let window = window_by_app_id(&mut f, "pin").unwrap();

    f.state().execute_action(&Action::TogglePinToScreen);
    assert!(
        f.state().is_pinned(&window),
        "precondition: window is pinned"
    );

    // The action's is_canvas_window filter drops pinned windows before toggle.
    f.state().execute_action(&Action::FillWindow);
    assert!(!f.state().stage.is_fill(&window));
}

#[test]
fn fill_already_filling_does_not_set_membership() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    let surface = map_settled(&mut f, id, "fill", (800, 600));
    let window = window_by_app_id(&mut f, "fill").unwrap();

    // Fill and adopt the filled geometry.
    f.state().toggle_fill_window(&window);
    f.double_roundtrip(id);
    let cw = f.client(id).window(&surface);
    let (w, h) = cw.configures_received.last().unwrap().1.size;
    cw.set_size(w as u16, h as u16);
    cw.ack_last_and_commit();
    f.double_roundtrip(id);

    // Drop the restore point (as a manual resize/move would), then fill again:
    // the window already fills its free space, so the geometry is a no-op and no
    // fill membership is recorded.
    f.state().stage.clear_fill(&window);
    f.state().fill_window(&window);
    assert!(!f.state().stage.is_fill(&window));
}

#[test]
fn fill_at_zoom_and_pan_uses_canvas_space_usable_area() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    let surface = map_settled(&mut f, id, "fill", (800, 600));
    let window = window_by_app_id(&mut f, "fill").unwrap();

    // Zoom to 0.5 and pan the camera: the usable area is a screen rect, so the
    // free canvas region fill grows into is (screen / zoom). Even numbers and
    // zoom 0.5 keep the screen→canvas conversion exact, dodging the 1px quirk.
    f.state().set_zoom(0.5);
    f.state().set_camera(Point::from((5000.0, 5000.0)));
    // Park the window inside the panned viewport so it intersects the bounds.
    f.state()
        .map_window(window.clone(), Point::from((6000, 6000)), false);

    f.state().toggle_fill_window(&window);
    f.double_roundtrip(id);

    // Canvas bounds = camera + screen/zoom = [5000,8840]×[5000,7160]; inset by a
    // 12px gap → free region 3816 × 2136 at canvas top-left (5012, 5012).
    let configures = f.client(id).window(&surface).format_recent_configures();
    assert!(
        configures.contains("size: 3816 × 2136"),
        "fill must configure the canvas-space free size, got:\n{configures}"
    );
    assert_eq!(
        f.state().stage.position_of(&window),
        Some(Point::from((5012, 5012))),
        "fill must map the window at the gap-inset canvas top-left"
    );
    assert!(f.state().stage.is_fill(&window));
}

fn config_ssd() -> Config {
    let mut config = Config::default();
    config.decorations.default_mode = DecorationMode::Server;
    config.decorations.border_width = 5;
    config
}

#[test]
fn fill_on_ssd_window_round_trips_bar_and_border() {
    let mut f = Fixture::with_config(config_ssd());
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    let surface = map_settled(&mut f, id, "fill", (800, 600));
    let window = window_by_app_id(&mut f, "fill").unwrap();
    // Precondition: the window actually carries a server-side title bar.
    assert_eq!(f.state().window_ssd_bar(&window), 25);

    // Pin the camera (mapping a window pans it to center) so the filled canvas
    // position is deterministic; keep the window inside the viewport.
    f.state().set_camera(Point::from((0.0, 0.0)));
    f.state()
        .map_window(window.clone(), Point::from((400, 300)), false);

    f.state().toggle_fill_window(&window);
    f.double_roundtrip(id);

    // The free frame region is 1896 × 1056 (usable minus a 12px gap). The client
    // content size is that minus a 5px border per side, and on height also the
    // 25px title bar: 1886 × 1021 — proving the chrome inflation round-trips.
    let configures = f.client(id).window(&surface).format_recent_configures();
    assert!(
        configures.contains("size: 1886 × 1021"),
        "fill must deflate the frame by border and bar, got:\n{configures}"
    );
    assert_eq!(
        f.state().stage.position_of(&window),
        Some(Point::from((17, 42))),
        "fill loc must offset the content by border and bar"
    );
    assert!(f.state().stage.is_fill(&window));
}

/// Fill must record its rect as the window's settled footprint. Leaving the
/// pre-fill rect cached makes every later commit read as "grew past settled" —
/// a perpetual reflow scan once the fill state is cleared (move-grab start,
/// nudge), and a real translation whenever the fill kept an unresolvable
/// overlap. A commit after the clear must leave the window in place.
#[test]
fn fill_records_settled_footprint() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    let a_surface = map_settled(&mut f, id, "a", (800, 600));
    let _b_surface = map_settled(&mut f, id, "b", (400, 1056));
    let a = window_by_app_id(&mut f, "a").unwrap();
    let b = window_by_app_id(&mut f, "b").unwrap();

    // Pin the camera (mapping pans it) and park A settled and gap-adjacent to
    // B: the settled adjacency is the reflow's anchor precondition.
    let gap = f.state().config.snap_gap as i32;
    f.state().set_camera(Point::from((0.0, 0.0)));
    f.state()
        .map_window(a.clone(), Point::from((400, 300)), false);
    f.state()
        .refresh_stable_snap_rect(&crate::state::StageWindow::Client(a.clone()));
    f.state()
        .map_window(b.clone(), Point::from((1200 + gap, 300)), false);

    f.state().toggle_fill_window(&a);
    assert!(f.state().stage.is_fill(&a), "fill must not silently no-op");
    f.double_roundtrip(id);
    let (w, h) = f
        .client(id)
        .window(&a_surface)
        .configures_received
        .last()
        .unwrap()
        .1
        .size;
    let win = f.client(id).window(&a_surface);
    win.set_size(w as u16, h as u16);
    win.attach_new_buffer();
    win.ack_last_and_commit();
    f.double_roundtrip(id);
    let filled_loc = f.state().stage.position_of(&a).unwrap();

    // The settled footprint is the filled frame, not the stale pre-fill rect.
    let a_id = super::server_surface(&a).id();
    let stable = f.state().stable_snap_rects.get(&a_id).copied().unwrap();
    assert_eq!(
        (stable.x_low, stable.y_low, stable.x_high, stable.y_high),
        (12.0, 12.0, 1200.0, 1068.0),
        "fill must cache its target rect as the settled footprint"
    );

    // Re-anchor: every move path (grab start, nudge, send-to-output) funnels
    // through clear_fill, then the app redraws before any grab-end settle.
    f.state().stage.clear_fill(&a);
    let win = f.client(id).window(&a_surface);
    win.attach_new_buffer();
    win.commit();
    f.double_roundtrip(id);

    assert_eq!(
        f.state().stage.position_of(&a),
        Some(filled_loc),
        "a redraw commit after clear_fill must not translate the filled window"
    );
}

/// The plain fullscreen round-trip (no straggler, no neighbor) must restore the
/// exact pre-fullscreen position — the reflow settle-guard must not disturb it.
#[test]
fn fullscreen_round_trip_restores_position() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    let surface = map_settled(&mut f, id, "fs", (800, 600));
    let window = window_by_app_id(&mut f, "fs").unwrap();
    let pre_pos = f.state().stage.position_of(&window).unwrap();

    // Fullscreen, then adopt the fullscreen size as a real client would.
    let cw = f.client(id).window(&surface);
    cw.set_fullscreen(None);
    f.double_roundtrip(id);
    adopt_last_configure(&mut f, id, &surface);

    // Exit and settle at the restored size.
    let cw = f.client(id).window(&surface);
    cw.unset_fullscreen();
    f.double_roundtrip(id);
    adopt_last_configure(&mut f, id, &surface);

    assert!(!f.state().stage.is_fullscreen(&window));
    assert_eq!(
        f.state().stage.position_of(&window),
        Some(pre_pos),
        "fullscreen round-trip must restore the exact pre-fullscreen position"
    );
}

/// A client exiting fullscreen keeps committing viewport-sized frames until it
/// acks the restore configure; those synchronous-exit stragglers read as "grown
/// past settled" against the stale pre-fullscreen rect. A gap-adjacent neighbor
/// must not get shoved aside by that reflow misread.
#[test]
fn fullscreen_exit_straggler_keeps_position() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    let a_surface = map_settled(&mut f, id, "a", (800, 600));
    let _b_surface = map_settled(&mut f, id, "b", (400, 1056));
    let a = window_by_app_id(&mut f, "a").unwrap();
    let b = window_by_app_id(&mut f, "b").unwrap();

    // Park A settled and gap-adjacent to B in canvas space: the settled
    // adjacency is the reflow's anchor precondition.
    let gap = f.state().config.snap_gap as i32;
    f.state()
        .map_window(a.clone(), Point::from((400, 300)), false);
    f.state()
        .refresh_stable_snap_rect(&StageWindow::Client(a.clone()));
    f.state()
        .map_window(b.clone(), Point::from((1200 + gap, 300)), false);
    let pre_pos = f.state().stage.position_of(&a).unwrap();

    // Client-initiated fullscreen, then adopt the fullscreen size.
    let window = f.client(id).window(&a_surface);
    window.set_fullscreen(None);
    f.double_roundtrip(id);
    adopt_last_configure(&mut f, id, &a_surface);

    // Exit runs synchronously server-side; the client has not acked yet.
    let window = f.client(id).window(&a_surface);
    window.unset_fullscreen();
    f.double_roundtrip(id);

    // Straggler: a still-fullscreen-sized frame lands before the restore
    // configure is acked.
    let window = f.client(id).window(&a_surface);
    window.attach_new_buffer();
    window.commit();
    f.double_roundtrip(id);

    // The client finally acks and settles at the restored size.
    adopt_last_configure(&mut f, id, &a_surface);

    assert_eq!(
        f.state().stage.position_of(&a),
        Some(pre_pos),
        "a straggler fullscreen-sized commit must not relocate the exiting window"
    );
}

/// Real clients (GTK4/celluloid) ack the restore configure as soon as they
/// process it, then keep committing old-fullscreen-sized frames for a frame
/// or two. Once acked, pending configures is empty, so the "unacked configure
/// differs from committed geometry" bail goes blind and the stale-sized
/// commit misreads as a grow-past-settled reflow.
#[test]
fn fullscreen_exit_early_ack_straggler_keeps_position() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    let a_surface = map_settled(&mut f, id, "a", (800, 600));
    let _b_surface = map_settled(&mut f, id, "b", (400, 1056));
    let a = window_by_app_id(&mut f, "a").unwrap();
    let b = window_by_app_id(&mut f, "b").unwrap();

    // Park A settled and gap-adjacent to B in canvas space: the settled
    // adjacency is the reflow's anchor precondition.
    let gap = f.state().config.snap_gap as i32;
    f.state()
        .map_window(a.clone(), Point::from((400, 300)), false);
    f.state()
        .refresh_stable_snap_rect(&StageWindow::Client(a.clone()));
    f.state()
        .map_window(b.clone(), Point::from((1200 + gap, 300)), false);
    let pre_pos = f.state().stage.position_of(&a).unwrap();

    // Client-initiated fullscreen, then adopt the fullscreen size.
    let window = f.client(id).window(&a_surface);
    window.set_fullscreen(None);
    f.double_roundtrip(id);
    adopt_last_configure(&mut f, id, &a_surface);

    // Exit runs synchronously server-side; the compositor sends the restore
    // configure.
    let window = f.client(id).window(&a_surface);
    window.unset_fullscreen();
    f.double_roundtrip(id);

    // Early ack: the client acks the restore configure immediately, before
    // resizing — pending configures is now empty.
    let window = f.client(id).window(&a_surface);
    window.ack_last();

    // Straggler: a still-fullscreen-sized frame (viewport destination
    // untouched) lands after the ack.
    let window = f.client(id).window(&a_surface);
    window.attach_new_buffer();
    window.commit();
    f.double_roundtrip(id);

    assert_eq!(
        f.state().stage.position_of(&a),
        Some(pre_pos),
        "a stale-sized frame committed after an early ack must not relocate the exiting window"
    );

    // Already acked above (the early ack), so this just draws the resize —
    // re-acking the same serial would be a protocol error.
    let (w, h) = f
        .client(id)
        .window(&a_surface)
        .configures_received
        .last()
        .unwrap()
        .1
        .size;
    let window = f.client(id).window(&a_surface);
    window.set_size(w as u16, h as u16);
    window.attach_new_buffer();
    window.commit();
    f.double_roundtrip(id);

    assert_eq!(
        f.state().stage.position_of(&a),
        Some(pre_pos),
        "the exiting window must settle back at its pre-fullscreen position"
    );
}

/// A client exiting fullscreen may settle at a size the compositor never
/// configured (an aspect-constrained player choosing its own dimensions).
/// The recenter must still land on the pre-fullscreen center, not some
/// adjacent-placement spot, and the settle must be over after that one commit.
#[test]
fn fullscreen_exit_settles_at_client_chosen_size_recentered() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    let a_surface = map_settled(&mut f, id, "a", (800, 600));
    let _b_surface = map_settled(&mut f, id, "b", (400, 1056));
    let a = window_by_app_id(&mut f, "a").unwrap();
    let b = window_by_app_id(&mut f, "b").unwrap();

    // Park A settled and gap-adjacent to B in canvas space: the settled
    // adjacency is the reflow's anchor precondition.
    let gap = f.state().config.snap_gap as i32;
    f.state()
        .map_window(a.clone(), Point::from((400, 300)), false);
    f.state()
        .refresh_stable_snap_rect(&StageWindow::Client(a.clone()));
    f.state()
        .map_window(b.clone(), Point::from((1200 + gap, 300)), false);
    let pre_pos = f.state().stage.position_of(&a).unwrap();

    // Client-initiated fullscreen, then adopt the fullscreen size.
    let window = f.client(id).window(&a_surface);
    window.set_fullscreen(None);
    f.double_roundtrip(id);
    adopt_last_configure(&mut f, id, &a_surface);

    // Exit runs synchronously server-side; the compositor sends the restore
    // configure.
    let window = f.client(id).window(&a_surface);
    window.unset_fullscreen();
    f.double_roundtrip(id);

    // Early ack: the client acks the restore configure immediately, before
    // resizing — pending configures is now empty.
    let window = f.client(id).window(&a_surface);
    window.ack_last();

    // The client settles at a size of its own choosing (700 × 500) instead of
    // the configured restore size (800 × 600) — already acked above, so this
    // just draws; re-acking the same serial would be a protocol error.
    let window = f.client(id).window(&a_surface);
    window.set_size(700, 500);
    window.attach_new_buffer();
    window.commit();
    f.double_roundtrip(id);

    let pre_exit_center = (pre_pos.x as f64 + 400.0, pre_pos.y as f64 + 300.0);
    let settled_pos = f.state().stage.position_of(&a).unwrap();
    let settled_center = (settled_pos.x as f64 + 350.0, settled_pos.y as f64 + 250.0);
    assert!(
        (settled_center.0 - pre_exit_center.0).abs() <= 2.0
            && (settled_center.1 - pre_exit_center.1).abs() <= 2.0,
        "client-chosen settle size must recenter on the pre-fullscreen center, \
         got {settled_center:?}, want {pre_exit_center:?}"
    );

    let window = f.client(id).window(&a_surface);
    window.attach_new_buffer();
    window.commit();
    f.double_roundtrip(id);
    assert_eq!(
        f.state().stage.position_of(&a),
        Some(settled_pos),
        "a repeat commit at the settled size must leave the position unchanged"
    );
}

/// A re-fullscreen mid-settle (before the client ever resizes down to the
/// restore size) must not let the outstanding recenter fire against the new
/// fullscreen geometry, and must not corrupt the position the window returns
/// to when it eventually exits fullscreen for real.
#[test]
fn refullscreen_during_exit_settle_keeps_return_position() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    let a_surface = map_settled(&mut f, id, "a", (800, 600));
    let _b_surface = map_settled(&mut f, id, "b", (400, 1056));
    let a = window_by_app_id(&mut f, "a").unwrap();
    let b = window_by_app_id(&mut f, "b").unwrap();

    // Park A settled and gap-adjacent to B in canvas space: the settled
    // adjacency is the reflow's anchor precondition.
    let gap = f.state().config.snap_gap as i32;
    f.state()
        .map_window(a.clone(), Point::from((400, 300)), false);
    f.state()
        .refresh_stable_snap_rect(&StageWindow::Client(a.clone()));
    f.state()
        .map_window(b.clone(), Point::from((1200 + gap, 300)), false);
    let pre_pos = f.state().stage.position_of(&a).unwrap();

    // Client-initiated fullscreen, then adopt the fullscreen size.
    let window = f.client(id).window(&a_surface);
    window.set_fullscreen(None);
    f.double_roundtrip(id);
    adopt_last_configure(&mut f, id, &a_surface);

    // Exit runs synchronously server-side; the compositor sends the restore
    // configure.
    let window = f.client(id).window(&a_surface);
    window.unset_fullscreen();
    f.double_roundtrip(id);

    // Early ack: the client acks the restore configure immediately, before
    // resizing — pending configures is now empty.
    let window = f.client(id).window(&a_surface);
    window.ack_last();

    // Stale fullscreen-sized straggler lands after the ack, mid-settle.
    let window = f.client(id).window(&a_surface);
    window.attach_new_buffer();
    window.commit();
    f.double_roundtrip(id);

    // The client re-fullscreens immediately, before ever resizing down to the
    // restore size — the exit settle is still outstanding.
    let window = f.client(id).window(&a_surface);
    window.set_fullscreen(None);
    f.double_roundtrip(id);
    adopt_last_configure(&mut f, id, &a_surface);

    assert!(
        f.state().stage.is_fullscreen(&a),
        "the re-fullscreen request must take effect despite the outstanding settle"
    );

    // Exit again — a fresh restore configure, safe to ack.
    let window = f.client(id).window(&a_surface);
    window.unset_fullscreen();
    f.double_roundtrip(id);
    adopt_last_configure(&mut f, id, &a_surface);

    assert_eq!(
        f.state().stage.position_of(&a),
        Some(pre_pos),
        "the mid-settle re-fullscreen must not corrupt the saved return position"
    );
}

/// A window mid fill-exit settle (a `pending_recenter` outstanding, client not
/// yet resized down) that then enters fullscreen must drop that recenter:
/// otherwise the settle completion fires on the first fullscreen-sized commit
/// and map_windows the now-fullscreen window off its output's camera origin,
/// breaking the fullscreen-parking invariant (`state/viewport.rs`).
#[test]
fn fullscreen_during_fill_exit_settle_stays_at_camera_origin() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    // Moving the camera (fill/fullscreen below) seeds a per-output blur
    // generation that only clears on output disconnect, so it can never return
    // to the pre-output baseline.
    f.skip_baseline_check();
    let id = f.add_client();

    let a_surface = map_settled(&mut f, id, "a", (800, 600));
    let _b_surface = map_settled(&mut f, id, "b", (400, 1056));
    let a = window_by_app_id(&mut f, "a").unwrap();
    let b = window_by_app_id(&mut f, "b").unwrap();

    // Pin the camera (mapping pans it) and park A gap-adjacent to B so fill has
    // real free space to grow into (a non-noop fill).
    let gap = f.state().config.snap_gap as i32;
    f.state().set_camera(Point::from((0.0, 0.0)));
    f.state()
        .map_window(a.clone(), Point::from((400, 300)), false);
    f.state()
        .refresh_stable_snap_rect(&StageWindow::Client(a.clone()));
    f.state()
        .map_window(b.clone(), Point::from((1200 + gap, 300)), false);

    // Fill A, then let the client adopt the filled size as a real client would.
    f.state().toggle_fill_window(&a);
    assert!(f.state().stage.is_fill(&a), "fill must not silently no-op");
    f.double_roundtrip(id);
    let (w, h) = f
        .client(id)
        .window(&a_surface)
        .configures_received
        .last()
        .unwrap()
        .1
        .size;
    let cw = f.client(id).window(&a_surface);
    cw.set_size(w as u16, h as u16);
    cw.ack_last_and_commit();
    f.double_roundtrip(id);

    // Unfill: registers a pending recenter whose pre_exit_size is the filled
    // size. Do NOT settle it — the client never resizes down.
    f.state().toggle_fill_window(&a);
    f.double_roundtrip(id);
    let a_id = super::server_surface(&a).id();
    assert!(
        f.state().pending_recenter.contains_key(&a_id),
        "unfill must register a settle to complete later"
    );

    // Enter fullscreen while that settle is still outstanding, then adopt the
    // fullscreen size. The fullscreen-sized commit differs from the outstanding
    // pre_exit_size, so a surviving recenter would fire against it.
    let cw = f.client(id).window(&a_surface);
    cw.set_fullscreen(None);
    f.double_roundtrip(id);
    adopt_last_configure(&mut f, id, &a_surface);

    assert!(f.state().stage.is_fullscreen(&a));
    let origin = f.state().camera().to_i32_round();
    assert_eq!(
        f.state().stage.position_of(&a),
        Some(origin),
        "a fullscreen window must stay parked at its camera origin, not be \
         recentered by a leftover fill-exit settle"
    );
    assert!(
        !f.state().pending_recenter.contains_key(&a_id),
        "entering fullscreen must drop the outstanding fill-exit recenter"
    );
}

/// A window that maps under a fullscreen window and itself requests fullscreen
/// before its first commit takes the deferred `pending_fullscreen` path:
/// background-placed, then fullscreened on the buffer commit. Its fullscreen
/// configure must carry Activated despite the un-activated placement.
#[test]
fn background_window_fullscreen_configure_is_activated() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    // A owns the output's fullscreen.
    let a_surface = map_settled(&mut f, id, "a", (800, 600));
    let cw = f.client(id).window(&a_surface);
    cw.set_fullscreen(None);
    f.double_roundtrip(id);
    adopt_last_configure(&mut f, id, &a_surface);

    // B requests fullscreen before its first commit, so the request is deferred;
    // it background-places under A, then the deferred branch takes it fullscreen.
    let b = f.client(id).create_window();
    let b_surface = b.surface.clone();
    b.set_app_id("b");
    b.set_fullscreen(None);
    b.commit();
    f.roundtrip(id);
    let b = f.client(id).window(&b_surface);
    b.set_size(400, 300);
    b.attach_new_buffer();
    b.ack_last_and_commit();
    f.double_roundtrip(id);

    let mapped = window_by_app_id(&mut f, "b").unwrap();
    assert_eq!(
        f.state().stage.fullscreen_output_of(&mapped),
        Some("HEADLESS-1"),
        "b must have taken over fullscreen via the deferred path"
    );
    let configures = f.client(id).window(&b_surface).format_recent_configures();
    let fs_line = configures
        .lines()
        .find(|l| l.contains("Fullscreen"))
        .unwrap_or("");
    assert!(
        fs_line.contains("Activated"),
        "b's fullscreen configure must carry Activated, got:\n{configures}"
    );
}
