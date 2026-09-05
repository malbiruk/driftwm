//! Window-effects animation bookkeeping. The stage/logical model always
//! updates instantly; these scenarios pin the *render-only* chase model that
//! lerps the drawn picture: open scale+fade, geometry chase toward a per-tick
//! live target, endpoint holds with an injectable deadline, fullscreen
//! visually-fullscreen gating, per-output scoping, and the crash/conversion
//! cleanup that drains the map.
//!
//! Backend is `None`, so anything that needs a renderer to exist (close
//! snapshots, crossfade overlays) never materializes — their counters stay 0,
//! and an assertion on them pins nothing. The capture half of a resize
//! crossfade is a plain map, though, so the lifecycle scenarios seed one
//! ([`seed_resize_capture`]) and the drop sites have to earn their zero; the
//! overlay half needs a texture and stays out of headless reach.
//! Everything else is driven through compositor-level entry points
//! (actions, fill/fit/fullscreen, commits, ticks) so the tests survive a refactor
//! of the private `WindowAnimations` internals. `tick_window_animations_at` takes
//! an injected `now` so endpoint deadlines are deterministic.

use std::time::{Duration, Instant};

use smithay::utils::{Logical, Point, Rectangle, Size};

use driftwm::config::{Action, BTN_LEFT, Config, Direction};
use driftwm::desktop_entry::DesktopEntryCache;
use driftwm::stage::ElementId;

use smithay::desktop::Window;
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
use wayland_client::protocol::wl_surface::WlSurface as ClientSurface;

use crate::state::window_animation::AnimSpace;
use crate::state::{StageWindow, SuspendedId};

use super::client::ClientId;
use super::real::TempDir;
use super::{
    Fixture, end_grab, map_window, motion, tick_until_settled, ticks_to_settle, window_by_app_id,
};

const TICK: Duration = Duration::from_millis(16);
const MAX_TICKS: usize = 600;
/// Comfortably past the 500ms endpoint-hold cap so an injected `now` releases it.
const PAST_HOLD: Duration = Duration::from_millis(600);

fn dist(a: Point<f64, Logical>, b: Point<f64, Logical>) -> f64 {
    let dx = a.x - b.x;
    let dy = a.y - b.y;
    (dx * dx + dy * dy).sqrt()
}

/// Active output at camera origin, zoom 1, with every camera animation quieted so
/// `output_has_active_animations` reflects only window animations.
fn reset_view(f: &mut Fixture) {
    f.state().with_output_state(|os| {
        os.camera = Point::from((0.0, 0.0));
        os.zoom = 1.0;
        os.camera_target = None;
        os.zoom_target = None;
        os.zoom_animation_anchor = None;
        os.overview_return = None;
        os.edge_pan_velocity = None;
        os.momentum.stop();
    });
    f.state().update_output_from_camera();
}

/// Opacity of the compositor chrome around `window`, exactly as the render loop
/// resolves it.
fn chrome_alpha(f: &mut Fixture, window: &Window) -> f32 {
    let id = f.state().stage.id_of(window);
    f.state().chrome_alpha_of(id, window)
}

fn element_id(f: &mut Fixture, window: &Window) -> ElementId {
    f.state()
        .stage
        .id_of(window)
        .expect("window is stage-mapped")
}

/// The stage element for the stand-in with id `sid`.
fn standin_element(f: &mut Fixture, sid: SuspendedId) -> StageWindow {
    f.state()
        .stage
        .windows()
        .find(|w| w.suspended().is_some_and(|s| s.id == sid))
        .cloned()
        .expect("the stand-in is on the stage")
}

/// Stage id of the stand-in for `sid` — the id an adoption hands on to the
/// window that takes over its slot.
fn standin_element_id(f: &mut Fixture, sid: SuspendedId) -> ElementId {
    let element = standin_element(f, sid);
    f.state().stage.id_of(&element).expect("and carries an id")
}

/// Put content in the stash a resize crossfade consumes, standing in for the
/// capture a headless fixture has no renderer to make. Every `resize_captures`
/// drop assertion needs this: with the map empty from the start it cannot tell a
/// working drop site from one that never ran. Stamped with the id's current
/// capture generation, or 0 when the id has no geometry entry — only the resolve
/// pairs on the stamp, the drop sites never look at it.
fn seed_resize_capture(f: &mut Fixture, id: ElementId) {
    let generation = f
        .state()
        .window_animations
        .generation_of(id)
        .unwrap_or_default();
    f.state().resize_captures.stash(
        id,
        crate::render::ClosePixels::empty(Rectangle::from_size(Size::from((400, 300)))),
        crate::render::BakeChrome {
            bare: true,
            corner_radius: [0.0; 4],
        },
        generation,
    );
    assert_eq!(
        f.state().debug_counters()["resize_captures"],
        1,
        "the seeded capture is in the map, so the drop below has something to do"
    );
}

/// Put a suspended "myapp" stand-in into the pending-relaunch state (the
/// "launching…" label) and hand back its id plus a client that has already
/// presented the relaunch token — one sized commit away from adopting the slot.
fn arrange_pending_relaunch(
    f: &mut Fixture,
    tmp: &TempDir,
) -> (SuspendedId, ClientId, ClientSurface) {
    std::fs::write(
        tmp.path().join("myapp.desktop"),
        "[Desktop Entry]\nType=Application\nName=myapp\nExec=myapp\n",
    )
    .unwrap();
    f.state().desktop_entry_cache = Some(DesktopEntryCache::new(vec![tmp.path().to_path_buf()]));

    let sid = f.state().insert_suspended_for_test(
        1,
        Point::from((500, 500)),
        Size::from((600, 400)),
        "myapp",
        "myapp",
    );
    f.state().relaunch_suspended(sid);
    let token = f.state().pending_relaunch_token_for_test(sid).unwrap();

    let cid = f.add_client();
    let win = f.client(cid).create_window();
    let surface = win.surface.clone();
    win.set_app_id("myapp");
    win.commit();
    f.roundtrip(cid);

    // Present the compositor-minted token before the first buffer (stash-for-adopt).
    f.client(cid).state.activation_token = Some(token);
    f.client(cid).activate(&surface);
    f.roundtrip(cid);

    (sid, cid, surface)
}

/// Relaunch a suspended "myapp" stand-in and adopt a freshly-mapped window into
/// its slot via the activation-token path. Returns the returning client.
fn adopt_relaunched(f: &mut Fixture, tmp: &TempDir) -> (ClientId, ClientSurface) {
    let (_sid, cid, surface) = arrange_pending_relaunch(f, tmp);

    // First sized commit adopts the stand-in's slot.
    let w = f.client(cid).window(&surface);
    w.set_size(300, 200);
    w.attach_new_buffer();
    w.ack_last_and_commit();
    f.double_roundtrip(cid);

    (cid, surface)
}

/// The first sized commit of a fresh window starts an open scale+fade: one entry,
/// alpha begins at 0 and the drawn size begins below the live size (scaled in).
#[test]
fn open_entry_appears_on_first_sized_commit() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    map_window(&mut f, id, "solo", (400, 300));
    let window = window_by_app_id(&mut f, "solo").unwrap();
    let eid = element_id(&mut f, &window);

    assert_eq!(
        f.state().window_animations.len(),
        1,
        "mapping starts exactly one animation"
    );
    assert!(
        f.state()
            .window_animations
            .geometry_visual_rect(eid)
            .is_none(),
        "a fresh map is an open entry, not a geometry chase"
    );

    let loc = f.state().stage.position_of(&window).unwrap().to_f64();
    let size = window.geometry().size.to_f64();
    let v = f.state().animated_visual(eid, loc, size);
    assert_eq!(v.alpha, 0.0, "the open fade starts fully transparent");
    assert!(
        v.size.w < size.w && v.size.h < size.h,
        "the window scales in from below its live size"
    );

    // It advances to completion and prunes.
    tick_until_settled(&mut f);
    assert_eq!(
        f.state().window_animations.len(),
        0,
        "the open entry pruned"
    );
}

/// An adopted (relaunched) window inherits the suspend crossfade, not an open
/// animation: its first sized commit is the adopt commit, which suppresses open.
#[test]
fn open_is_suppressed_for_an_adopted_window() {
    let tmp = TempDir::new();
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    let (_cid, _surface) = adopt_relaunched(&mut f, &tmp);
    let adopted = window_by_app_id(&mut f, "myapp").expect("the window adopted the slot");
    let eid = element_id(&mut f, &adopted);

    // The only entry is the geometry hold that keeps it filling the slot — an
    // open entry would report no geometry rect and would fade the window in.
    assert!(
        f.state()
            .window_animations
            .geometry_visual_rect(eid)
            .is_some(),
        "an adopted window gets a geometry hold, not an open scale+fade"
    );
    let loc = f.state().stage.position_of(&adopted).unwrap().to_f64();
    let size = adopted.geometry().size.to_f64();
    assert_eq!(
        f.state().animated_visual(eid, loc, size).alpha,
        1.0,
        "the adopted window renders opaque — no fade in flight"
    );
}

/// A second move action mid-flight retargets the same entry (its visual is kept
/// and only the target changes), so the drawn path never jumps: on every tick the
/// visual advances no further than the straight-line distance still remaining to
/// the current live target.
#[test]
fn a_second_action_mid_flight_keeps_the_visual_path_continuous() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    map_window(&mut f, id, "a", (400, 300));
    let window = window_by_app_id(&mut f, "a").unwrap();
    reset_view(&mut f);
    f.state()
        .map_window(window.clone(), Point::from((400, 300)), false);
    let eid = element_id(&mut f, &window);
    tick_until_settled(&mut f); // drain the open entry

    // First move: (400,300) → (900,300).
    f.state()
        .map_window(window.clone(), Point::from((900, 300)), false);
    f.state()
        .animate_window_move_from(&window, Point::from((400, 300)), None);

    let continuous_tick = |f: &mut Fixture| {
        let target = f.state().stage.position_of(&window).unwrap().to_f64();
        let before = f
            .state()
            .window_animations
            .geometry_visual_rect(eid)
            .expect("geometry entry in flight")
            .loc;
        let remaining = dist(before, target);
        f.state().tick_window_animations(TICK);
        let after = f
            .state()
            .window_animations
            .geometry_visual_rect(eid)
            .map(|r| r.loc)
            .unwrap_or(target); // pruned this tick == arrived at target
        assert!(
            dist(after, before) <= remaining + 1e-6,
            "the visual jumped {:.3} with only {:.3} left to the target — discontinuous",
            dist(after, before),
            remaining
        );
    };

    for _ in 0..4 {
        continuous_tick(&mut f);
    }

    // Interruption: retarget to a far, different point mid-flight.
    f.state()
        .map_window(window.clone(), Point::from((200, 700)), false);
    f.state()
        .animate_window_move_from(&window, Point::from((900, 300)), None);

    for _ in 0..MAX_TICKS {
        if !f.state().window_animations.is_active() {
            return;
        }
        continuous_tick(&mut f);
    }
    panic!("the interrupted animation never converged");
}

/// The target is re-read every tick: moving the window's stage position mid-flight
/// bends the visual toward the new target without snapping to it.
#[test]
fn mid_flight_map_window_retargets_without_snapping() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    map_window(&mut f, id, "a", (400, 300));
    let window = window_by_app_id(&mut f, "a").unwrap();
    reset_view(&mut f);
    f.state()
        .map_window(window.clone(), Point::from((400, 300)), false);
    let eid = element_id(&mut f, &window);
    tick_until_settled(&mut f);

    f.state()
        .map_window(window.clone(), Point::from((900, 300)), false);
    f.state()
        .animate_window_move_from(&window, Point::from((400, 300)), None);
    for _ in 0..3 {
        f.state().tick_window_animations(TICK);
    }
    let before = f
        .state()
        .window_animations
        .geometry_visual_rect(eid)
        .unwrap()
        .loc;
    assert!(
        before.x > 400.0 && before.x < 900.0,
        "mid-flight, the visual sits between start and target"
    );

    // Retarget far the other way; the stage moved, the animate call is not repeated.
    f.state()
        .map_window(window.clone(), Point::from((100, 300)), false);
    f.state().tick_window_animations(TICK);
    let after = f
        .state()
        .window_animations
        .geometry_visual_rect(eid)
        .unwrap()
        .loc;
    assert!(
        after.x < before.x,
        "the path bent toward the new (lower-x) target"
    );
    assert!(
        after.x > 101.0,
        "it lerped a fraction of the way, it did not snap to the target"
    );

    tick_until_settled(&mut f);
}

/// Geometry animations run on normalized progress, so their duration is a fixed
/// number of ticks regardless of how far the window travels — and it matches the
/// open animation's. A distance-epsilon chase instead grows a log-distance tail,
/// which is what made fit/fill/fullscreen feel slower than open/close.
#[test]
fn geometry_settles_in_the_same_time_regardless_of_distance() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    map_window(&mut f, id, "a", (400, 300));
    let window = window_by_app_id(&mut f, "a").unwrap();
    reset_view(&mut f);

    // The open animation's duration is the reference.
    f.state()
        .map_window(window.clone(), Point::from((100, 300)), false);
    f.state().start_window_open_animation(&window);
    let open_ticks = ticks_to_settle(&mut f);
    assert!(open_ticks > 2, "the open animation should take real time");

    // A short hop, then a hop ~60x longer. Both stay inside the viewport so the
    // travelling visual never loses eligibility (which would end it instantly).
    let mut move_ticks = Vec::new();
    for (from, to) in [((100, 300), (120, 300)), ((100, 300), (1300, 300))] {
        f.state()
            .map_window(window.clone(), Point::from(from), false);
        tick_until_settled(&mut f);
        f.state().map_window(window.clone(), Point::from(to), false);
        f.state()
            .animate_window_move_from(&window, Point::from(from), None);
        assert!(
            f.state().window_animations.is_active(),
            "the move started a chase"
        );
        move_ticks.push(ticks_to_settle(&mut f));
    }

    assert_eq!(
        move_ticks[0], move_ticks[1],
        "a 20px and a 1200px move must take the same number of ticks, got {move_ticks:?}"
    );
    assert!(
        move_ticks[0].abs_diff(open_ticks) <= 2,
        "geometry ({}) and open ({open_ticks}) should settle in the same time",
        move_ticks[0],
    );
}

/// A size request equal to the committed size is resolved at the start (never
/// rides to the endpoint hold) — the entry prunes on convergence like a
/// position-only move, so `tick_until_settled` returns instead of spinning.
#[test]
fn size_request_equal_to_committed_never_holds() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    map_window(&mut f, id, "a", (400, 300));
    let window = window_by_app_id(&mut f, "a").unwrap();
    reset_view(&mut f);
    f.state()
        .map_window(window.clone(), Point::from((400, 300)), false);
    let eid = element_id(&mut f, &window);
    tick_until_settled(&mut f);

    let committed = window.geometry().size;
    // Request the size the window already has, then move it — the size request
    // must be discarded, leaving a pure position chase that settles.
    f.state().animate_window_geometry(&window, committed, None);
    f.state()
        .map_window(window.clone(), Point::from((700, 300)), false);
    assert!(
        f.state()
            .window_animations
            .geometry_visual_rect(eid)
            .is_some(),
        "the move started a geometry chase"
    );

    tick_until_settled(&mut f);
    assert_eq!(
        f.state().window_animations.len(),
        0,
        "an equal-size request settled instead of holding at the endpoint"
    );
}

/// A client that never redraws is bounded twice over: the start hold freezes the
/// window for its budget, the leg then runs with stale (capped) content, and the
/// endpoint hold bounds the wait at the far end before the entry finally prunes.
/// Neither deadline can strand an entry.
#[test]
fn an_unacked_request_is_bounded_by_both_deadlines() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    map_window(&mut f, id, "a", (400, 300));
    let window = window_by_app_id(&mut f, "a").unwrap();
    reset_view(&mut f);
    f.state()
        .map_window(window.clone(), Point::from((400, 300)), false);
    let eid = element_id(&mut f, &window);
    tick_until_settled(&mut f);

    let committed = window.geometry().size;
    let bigger = Size::from((committed.w + 300, committed.h + 300));
    let seed = f.state().stage.position_of(&window).unwrap().to_f64();
    f.state().animate_window_geometry(&window, bigger, None);
    f.state()
        .map_window(window.clone(), Point::from((700, 300)), false);

    // Frozen: the request is outstanding and nothing has moved.
    let base = Instant::now();
    for _ in 0..60 {
        f.state().tick_window_animations_at(TICK, base);
    }
    assert!(f.state().window_animations.start_held(eid), "still frozen");
    assert!(
        f.state().has_active_animations(),
        "a start hold counts as an active animation"
    );
    let held = f
        .state()
        .window_animations
        .geometry_visual_rect(eid)
        .unwrap();
    assert!(
        dist(held.loc, seed) <= 0.5,
        "nothing moved while frozen ({held:?})"
    );

    // Past the start budget the leg runs and parks at the endpoint.
    let after_start = base + PAST_HOLD;
    for _ in 0..MAX_TICKS {
        f.state().tick_window_animations_at(TICK, after_start);
        if !f.state().window_animations.start_held(eid) {
            break;
        }
    }
    assert!(
        !f.state().window_animations.start_held(eid),
        "the start budget expired"
    );
    for _ in 0..60 {
        f.state().tick_window_animations_at(TICK, after_start);
    }
    let parked = f
        .state()
        .window_animations
        .geometry_visual_rect(eid)
        .expect("the endpoint hold keeps it alive");
    assert!(
        (parked.size.w - bigger.w as f64).abs() <= 0.5,
        "the leg ran to the requested endpoint ({parked:?})"
    );

    // And past the endpoint budget too, it finally prunes.
    let after_endpoint = after_start + PAST_HOLD;
    for _ in 0..MAX_TICKS {
        if !f.state().window_animations.is_active() {
            break;
        }
        f.state().tick_window_animations_at(TICK, after_endpoint);
    }
    assert_eq!(
        f.state().window_animations.len(),
        0,
        "both deadlines fired, so the entry pruned"
    );
}

/// A commit at the requested size is what the freeze is waiting for: it releases
/// the hold, the leg runs with the client's real new content, and the entry
/// prunes.
#[test]
fn a_commit_resolves_the_outstanding_request() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let surface = map_window(&mut f, id, "a", (800, 600));
    let window = window_by_app_id(&mut f, "a").unwrap();
    reset_view(&mut f);
    f.state()
        .map_window(window.clone(), Point::from((400, 300)), false);
    let eid = element_id(&mut f, &window);
    tick_until_settled(&mut f); // drain open

    // An outstanding size request the client has not yet committed.
    let committed = window.geometry().size;
    let requested = Size::from((committed.w + 100, committed.h + 100));
    f.state().animate_window_geometry(&window, requested, None);
    let base = Instant::now();
    for _ in 0..60 {
        f.state().tick_window_animations_at(TICK, base);
    }
    assert!(
        f.state().window_animations.is_active(),
        "the entry is frozen, waiting for the client to redraw"
    );
    // The old picture the freeze has been holding on screen.
    seed_resize_capture(&mut f, eid);

    // The client commits a buffer at the requested size — a clean ack resolves it.
    let w = f.client(id).window(&surface);
    w.set_size(requested.w as u16, requested.h as u16);
    w.attach_new_buffer();
    w.ack_last_and_commit();
    f.double_roundtrip(id);
    let counters = f.state().debug_counters();
    assert_eq!(
        counters["resize_captures"], 0,
        "the resolve consumed the stashed old picture — the one moment old and \
         new content both exist"
    );
    assert_eq!(
        counters["resize_crossfades"], 0,
        "the overlay it would have become needs a renderer, so only the consume \
         side is pinned headless"
    );
    tick_until_settled(&mut f);
    assert_eq!(
        f.state().window_animations.len(),
        0,
        "the commit resolved the request and the chase pruned"
    );
}

/// A commit to a size the compositor never requested (the client chose its own —
/// reality wins) resolves the request just like a clean ack: the hold ends and
/// the chase bends to live.
#[test]
fn a_client_chosen_size_also_resolves_the_request() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let surface = map_window(&mut f, id, "a", (800, 600));
    let window = window_by_app_id(&mut f, "a").unwrap();
    reset_view(&mut f);
    f.state()
        .map_window(window.clone(), Point::from((400, 300)), false);
    let eid = element_id(&mut f, &window);
    tick_until_settled(&mut f);

    let committed = window.geometry().size;
    let requested = Size::from((committed.w + 200, committed.h + 200));
    f.state().animate_window_geometry(&window, requested, None);
    let base = Instant::now();
    for _ in 0..60 {
        f.state().tick_window_animations_at(TICK, base);
    }
    assert!(
        f.state().window_animations.is_active(),
        "the request is outstanding, so the entry is still frozen"
    );
    seed_resize_capture(&mut f, eid);

    // The client commits a third size — neither the request nor the prior size.
    let chosen: Size<i32, Logical> = Size::from((committed.w + 50, committed.h + 50));
    let w = f.client(id).window(&surface);
    w.set_size(chosen.w as u16, chosen.h as u16);
    w.attach_new_buffer();
    w.commit();
    f.double_roundtrip(id);
    let counters = f.state().debug_counters();
    assert_eq!(
        counters["resize_captures"], 0,
        "a client-chosen size resolves the freeze too, consuming the old picture"
    );
    assert_eq!(counters["resize_crossfades"], 0, "overlay is backend-gated");
    tick_until_settled(&mut f);
    assert_eq!(
        f.state().window_animations.len(),
        0,
        "the client-chosen size resolved the request and the chase pruned"
    );
}

/// A pinned window's entry chases in screen space, so a camera pan (which rewrites
/// a pinned window's canvas stage loc every tick) does not churn it. Driven
/// through the real fullscreen-exit re-pin path, which seats a screen-space entry.
#[test]
fn pinned_entry_chases_in_screen_space_and_survives_a_pan() {
    let mut f = Fixture::with_config(
        Config::from_toml("[[window_rules]]\napp_id = \"p\"\npinned_to_screen = true\n").unwrap(),
    );
    let output = f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let surface = map_window(&mut f, id, "p", (400, 300));
    let window = window_by_app_id(&mut f, "p").unwrap();
    let eid = element_id(&mut f, &window);
    assert!(f.state().is_pinned(&window), "the window pinned via rule");

    // Enter fullscreen (unpins) and let the client ack the viewport size, so the
    // saved (pre-fullscreen) size differs from the committed one — the exit entry
    // then carries a real outstanding request and holds rather than pruning.
    f.state().enter_fullscreen(&window, Some(output.clone()));
    assert!(
        !f.state().is_pinned(&window),
        "fullscreen unpins the window"
    );
    f.double_roundtrip(id);
    super::adopt_last_configure(&mut f, id, &surface);

    // Exit re-pins and seats a screen-space geometry entry.
    f.state().exit_fullscreen_on(&output);
    assert!(
        f.state().is_pinned(&window),
        "exit re-pins the window to its site"
    );
    assert!(
        f.state().window_animations.is_active(),
        "the exit seated a geometry entry"
    );

    // Converge under a fixed now (so the hold never times out), then pan far.
    let base = Instant::now();
    for _ in 0..60 {
        f.state().tick_window_animations_at(TICK, base);
    }
    let before = f
        .state()
        .window_animations
        .geometry_visual_rect(eid)
        .unwrap();

    f.state().set_camera(Point::from((6000.0, 6000.0)));
    f.state().update_output_from_camera();
    f.state().tick_window_animations_at(TICK, base);
    let after = f
        .state()
        .window_animations
        .geometry_visual_rect(eid)
        .unwrap();

    assert!(
        dist(after.loc, before.loc) <= 0.5,
        "a camera pan did not churn the screen-space pinned entry ({before:?} → {after:?})"
    );
}

/// A position-only nudge starts a geometry chase that prunes once it converges.
#[test]
fn position_only_nudge_prunes_on_convergence() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    map_window(&mut f, id, "a", (400, 300));
    let window = window_by_app_id(&mut f, "a").unwrap();
    reset_view(&mut f);
    f.state()
        .map_window(window.clone(), Point::from((400, 300)), false);
    let eid = element_id(&mut f, &window);
    tick_until_settled(&mut f);
    let serial = smithay::utils::SERIAL_COUNTER.next_serial();
    f.state().raise_and_focus(&window, serial);

    f.state()
        .execute_action(&Action::NudgeWindow(Direction::Right));
    assert!(
        f.state()
            .window_animations
            .geometry_visual_rect(eid)
            .is_some(),
        "a nudge starts a position-only geometry chase"
    );

    tick_until_settled(&mut f);
    assert_eq!(
        f.state().window_animations.len(),
        0,
        "the position-only chase pruned on convergence"
    );
}

/// A window under an active interactive grab gets no animation entry: the grab
/// guard suppresses the start (the same guard shared by open and every geometry
/// site).
#[test]
fn no_entry_starts_under_an_interactive_grab() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    map_window(&mut f, id, "a", (400, 300));
    let window = window_by_app_id(&mut f, "a").unwrap();
    reset_view(&mut f);
    f.state()
        .map_window(window.clone(), Point::from((400, 300)), false);
    tick_until_settled(&mut f);
    let serial = smithay::utils::SERIAL_COUNTER.next_serial();
    f.state().raise_and_focus(&window, serial);

    f.state().arm_interactive_move(&window);
    f.state()
        .execute_action(&Action::NudgeWindow(Direction::Right));
    assert_eq!(
        f.state().window_animations.len(),
        0,
        "no geometry entry started while the window was grabbed"
    );
    f.state().disarm_interactive_move(&window);
}

/// An output is visually fullscreen only once the fullscreen-entry animation
/// finishes: false mid-entry, true after the client acks and the chase prunes.
#[test]
fn output_is_visually_fullscreen_only_after_the_entry_finishes() {
    let mut f = Fixture::new();
    let output = f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let surface = map_window(&mut f, id, "fs", (800, 600));
    let window = window_by_app_id(&mut f, "fs").unwrap();
    reset_view(&mut f);
    let eid = element_id(&mut f, &window);

    // Client-requested fullscreen: the enter replaces the open entry with a
    // fullscreen-entry geometry chase.
    f.client(id).window(&surface).set_fullscreen(None);
    f.double_roundtrip(id);
    assert!(
        f.state().is_output_fullscreen(&output),
        "the output is logically fullscreen"
    );
    assert!(
        f.state().window_animations.fullscreen_entry_active(eid),
        "a fullscreen-entry animation is in flight"
    );
    assert!(
        !f.state().is_output_visually_fullscreen(&output),
        "the output is not YET visually fullscreen mid-entry"
    );

    // Ack the fullscreen size and run the chase to completion.
    super::adopt_last_configure(&mut f, id, &surface);
    tick_until_settled(&mut f);
    assert!(
        f.state().is_output_visually_fullscreen(&output),
        "the output is visually fullscreen once the entry converges"
    );

    f.state().exit_fullscreen_on(&output);
}

/// The settled predicate also carries a camera-scoped claim, mirroring the
/// frozen picture's: the window is mapped at the camera origin, so a viewport
/// that has drifted off that origin is one the window no longer covers, and
/// claiming coverage there would cull the canvas out from under it.
#[test]
fn a_camera_drifted_off_the_fullscreen_park_is_no_longer_visually_covered() {
    let mut f = Fixture::new();
    let output = f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let surface = map_window(&mut f, id, "fs", (800, 600));
    reset_view(&mut f);

    f.client(id).window(&surface).set_fullscreen(None);
    f.double_roundtrip(id);
    super::adopt_last_configure(&mut f, id, &surface);
    tick_until_settled(&mut f);
    assert!(
        f.state().is_output_visually_fullscreen(&output),
        "precondition: the settled entry covers the output"
    );

    f.state().with_output_state(|os| os.camera.x += 1.0);
    assert!(
        !f.state().is_output_visually_fullscreen(&output),
        "a camera that drifted off the park origin covers nothing"
    );

    f.state().exit_fullscreen_on(&output);
}

/// The park comparison is exact, not epsilon: it writes integers and exactly
/// 1.0, so even a sub-pixel drift is a real seam to catch, not rounding noise
/// to tolerate.
#[test]
fn a_subpixel_camera_drift_off_the_fullscreen_park_also_ends_coverage() {
    let mut f = Fixture::new();
    let output = f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let surface = map_window(&mut f, id, "fs", (800, 600));
    reset_view(&mut f);

    f.client(id).window(&surface).set_fullscreen(None);
    f.double_roundtrip(id);
    super::adopt_last_configure(&mut f, id, &surface);
    tick_until_settled(&mut f);

    f.state().with_output_state(|os| os.camera.x += 0.001);
    assert!(
        !f.state().is_output_visually_fullscreen(&output),
        "a hair of drift must not be tolerated as rounding noise"
    );

    f.state().exit_fullscreen_on(&output);
}

/// The settled predicate's camera-scoped claim is a conjunction, not just the
/// camera half of it: a zoom that drifted off 1.0 while the camera stayed
/// exactly on the park origin must also end the coverage claim — the window is
/// mapped at zoom 1, so anything else no longer matches what is on screen.
#[test]
fn a_zoom_drifted_off_the_fullscreen_park_is_no_longer_visually_covered() {
    let mut f = Fixture::new();
    let output = f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let surface = map_window(&mut f, id, "fs", (800, 600));
    reset_view(&mut f);

    f.client(id).window(&surface).set_fullscreen(None);
    f.double_roundtrip(id);
    super::adopt_last_configure(&mut f, id, &surface);
    tick_until_settled(&mut f);
    assert!(
        f.state().is_output_visually_fullscreen(&output),
        "precondition: the settled entry covers the output"
    );

    f.state().with_output_state(|os| os.zoom += 0.001);
    assert!(
        !f.state().is_output_visually_fullscreen(&output),
        "a zoom that drifted off the park's 1.0 covers nothing, even with the \
         camera still exactly parked"
    );

    f.state().exit_fullscreen_on(&output);
}

/// The park to zoom 1 is fullscreen's business, not the scene's: while the
/// entering window grows, everything behind it keeps rendering through the
/// pre-fullscreen view, and only follows the park once the window covers the
/// output — by which point the scene is culled outright and never has to travel.
#[test]
fn the_scene_keeps_its_view_until_the_fullscreen_entry_lands() {
    let mut f = Fixture::new();
    let output = f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let surface = map_window(&mut f, id, "fs", (800, 600));
    reset_view(&mut f);
    f.state().with_output_state(|os| {
        os.camera = Point::from((40.0, 25.0));
        os.zoom = 0.5;
    });
    f.state().update_output_from_camera();
    let pre = f
        .state()
        .with_output_state(|os| (os.camera, os.zoom))
        .unwrap();
    assert_eq!(
        f.state().world_view(&output),
        pre,
        "with nothing in flight the scene is on the live viewport"
    );

    f.client(id).window(&surface).set_fullscreen(None);
    f.double_roundtrip(id);
    let parked = f
        .state()
        .with_output_state(|os| (os.camera, os.zoom))
        .unwrap();
    assert_eq!(parked.1, 1.0, "the viewport parked at zoom 1");
    assert_ne!(parked, pre, "the park moved the live viewport");
    assert_eq!(
        f.state().world_view(&output),
        pre,
        "the scene stays on the pre-fullscreen view for the whole entry"
    );

    super::adopt_last_configure(&mut f, id, &surface);
    tick_until_settled(&mut f);
    assert_eq!(
        f.state().world_view(&output),
        parked,
        "the scene follows the park once the entry lands"
    );

    f.state().exit_fullscreen_on(&output);
}

/// A frozen fullscreen picture claims to cover its output, and that claim culls
/// everything underneath. It is only good while the output still shows the view
/// the picture froze under: a pan gesture, its momentum or a navigation action
/// during the exit's freeze slides the picture off that output, and holding the
/// claim through that would leave the pan crossing black.
#[test]
fn a_camera_move_ends_a_frozen_fullscreen_cover() {
    let mut f = Fixture::new();
    let output = f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let surface = map_window(&mut f, id, "fs", (800, 600));
    reset_view(&mut f);

    f.client(id).window(&surface).set_fullscreen(None);
    f.double_roundtrip(id);
    super::adopt_last_configure(&mut f, id, &surface);
    tick_until_settled(&mut f);

    f.state().exit_fullscreen_on(&output);
    assert!(
        f.state().is_output_visually_fullscreen(&output),
        "the exit's freeze still holds the fullscreen picture on the output"
    );

    f.state().with_output_state(|os| os.camera.x += 300.0);
    assert!(
        !f.state().is_output_visually_fullscreen(&output),
        "a picture the camera has panned away from covers nothing"
    );
}

/// Reversing out of a fullscreen entry mid-flight seeds the exit from the entry's
/// current visual, frame-converted back to the restored camera space — so the
/// on-screen picture is continuous (no jump) and the fullscreen-entry role clears.
/// Driven at a pre-fullscreen zoom ≠ 1, where a keep-the-locked-space-visual bug
/// (invisible at zoom 1's identity conversion) would jump the window on exit.
#[test]
fn exit_from_mid_entry_continues_from_the_current_visual() {
    let mut f = Fixture::new();
    let output = f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let surface = map_window(&mut f, id, "fs", (800, 600));
    let window = window_by_app_id(&mut f, "fs").unwrap();
    reset_view(&mut f);
    f.state().with_output_state(|os| os.zoom = 2.0);
    f.state().update_output_from_camera();
    f.state()
        .map_window(window.clone(), Point::from((100, 100)), false);
    let eid = element_id(&mut f, &window);
    // Let the window finish opening: a fullscreen armed while its open fade has
    // never been drawn takes that fade over and seeds at the fullscreen rect,
    // which is a different transition from the grow this test drives.
    tick_until_settled(&mut f);

    f.client(id).window(&surface).set_fullscreen(None);
    f.double_roundtrip(id);
    let locked_camera = f.state().with_output_state(|os| os.camera).unwrap();
    // The enter freezes until the client redraws at the fullscreen size. Ack it,
    // then advance the leg partway, so the exit interrupts a rect in motion
    // rather than one still sitting on its seed.
    let seed = f
        .state()
        .window_animations
        .geometry_visual_rect(eid)
        .unwrap();
    super::adopt_last_configure(&mut f, id, &surface);
    f.state().tick_window_animations(TICK);
    f.state().tick_window_animations(TICK);
    let mid = f
        .state()
        .window_animations
        .geometry_visual_rect(eid)
        .unwrap();
    assert!(
        mid.size.w > seed.size.w + 1.0 && mid.size.w < 1919.0,
        "the entry is genuinely mid-flight ({seed:?} -> {mid:?})"
    );
    assert!(f.state().window_animations.fullscreen_entry_active(eid));
    // The mid visual's on-screen position in the locked (zoom-1) viewport.
    let mid_screen = mid.loc - locked_camera;

    f.state().exit_fullscreen_on(&output);
    assert!(
        !f.state().is_output_fullscreen(&output),
        "fullscreen is logically gone after exit"
    );
    assert!(
        !f.state().window_animations.fullscreen_entry_active(eid),
        "the fullscreen-entry role cleared on exit"
    );
    let (restored_camera, restored_zoom) = f
        .state()
        .with_output_state(|os| (os.camera, os.zoom))
        .unwrap();
    let after = f
        .state()
        .window_animations
        .geometry_visual_rect(eid)
        .expect("the exit continues a geometry entry");
    // Same on-screen position under the restored camera/zoom — continuous.
    let after_screen = Point::from((
        (after.loc.x - restored_camera.x) * restored_zoom,
        (after.loc.y - restored_camera.y) * restored_zoom,
    ));
    assert!(
        dist(after_screen, mid_screen) <= 1.5,
        "the exit stayed screen-continuous across the zoom change ({mid_screen:?} → {after_screen:?})"
    );
}

/// Converting a live window to a suspended stand-in (suspend action + real close)
/// drops the window's animation entry — the stand-in inherits the id but no chase.
#[test]
fn conversion_drops_the_window_animation_entry() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let surface = map_window(&mut f, id, "myapp", (400, 300));
    let window = window_by_app_id(&mut f, "myapp").unwrap();
    let serial = smithay::utils::SERIAL_COUNTER.next_serial();
    f.state().raise_and_focus(&window, serial);
    let eid = element_id(&mut f, &window);
    assert_eq!(
        f.state().window_animations.len(),
        1,
        "the mapped window has an open entry pre-conversion"
    );
    seed_resize_capture(&mut f, eid);

    // Suspend then close: the destroy converts the window into a stand-in.
    f.state().execute_action(&Action::SuspendWindow);
    f.client(id).window(&surface).destroy();
    f.roundtrip(id);
    f.dispatch();

    assert_eq!(
        f.state().window_animations.len(),
        0,
        "conversion dropped the entry for the converted id"
    );
    let counters = f.state().debug_counters();
    assert_eq!(counters["closing_snapshots"], 0);
    assert_eq!(counters["standin_fades"], 0);
    assert_eq!(counters["close_pixels"], 0);
    assert_eq!(
        counters["resize_captures"], 0,
        "the id died with the client here, so the teardown sweep collects its \
         seeded capture (the in-place conversion is pinned below)"
    );
    assert_eq!(counters["resize_crossfades"], 0, "overlay is backend-gated");

    // Tear the stand-in down for the baseline.
    let sid = f
        .state()
        .stage
        .windows()
        .find_map(|w| w.suspended().map(|s| s.id));
    if let Some(sid) = sid {
        f.state().dismiss_suspended(sid);
    }
}

/// Adoption drops both stale window-animation entries and replaces them with a
/// single hold on the adopted window's slot, and content stashed against the
/// stand-in's id goes with them — the adopted window inherits that id, so no
/// sweep would ever collect it.
#[test]
fn adoption_drops_entries_and_creates_no_render_transient() {
    let tmp = TempDir::new();
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    let (sid, cid, surface) = arrange_pending_relaunch(&mut f, &tmp);
    let standin_id = standin_element_id(&mut f, sid);
    seed_resize_capture(&mut f, standin_id);

    // The first sized commit adopts the stand-in's slot.
    let w = f.client(cid).window(&surface);
    w.set_size(300, 200);
    w.attach_new_buffer();
    w.ack_last_and_commit();
    f.double_roundtrip(cid);
    assert!(
        window_by_app_id(&mut f, "myapp").is_some(),
        "the window adopted the slot"
    );

    // Exactly one entry: the adopted window's slot hold. Neither involved id
    // carried a stale chase across the replace.
    let adopted = window_by_app_id(&mut f, "myapp").unwrap();
    let eid = element_id(&mut f, &adopted);
    assert_eq!(
        f.state().window_animations.len(),
        1,
        "adoption leaves only the adopted window's hold"
    );
    assert!(
        f.state()
            .window_animations
            .geometry_visual_rect(eid)
            .is_some(),
        "and that entry belongs to the adopted window"
    );
    let counters = f.state().debug_counters();
    assert_eq!(
        counters["standin_fades"], 0,
        "the adoption crossfade is backend-gated — none headless"
    );
    assert_eq!(counters["closing_snapshots"], 0);
    assert_eq!(counters["close_pixels"], 0);
    // Adoption replaces the stand-in in place, keeping its id: same hazard as
    // conversion, so both involved ids' crossfade halves go at the replace.
    assert_eq!(
        counters["resize_captures"], 0,
        "the seeded capture went at the replace, not on a sweep that cannot fire"
    );
    assert_eq!(counters["resize_crossfades"], 0, "overlay is backend-gated");
}

/// The stand-in's "launching…" label state is live while the relaunch is pending
/// and gone the moment the window adopts the slot. The adoption crossfade renders
/// the departed stand-in's chrome, so it must capture this state when the fade is
/// created — a live lookup at render time reads the post-adopt value, re-keys the
/// cached label buffer, and the fade visibly swaps to the plain name before
/// fading. This pins the ordering the capture depends on; the frozen pixels
/// themselves are render-only (the fade is backend-gated, none exists headless).
#[test]
fn adoption_clears_the_launching_state_the_fade_captures() {
    let tmp = TempDir::new();
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    let (sid, cid, surface) = arrange_pending_relaunch(&mut f, &tmp);
    assert!(
        f.state().is_suspended_launching(sid),
        "the stand-in shows the launching label while the relaunch is pending"
    );

    // The adopting sized commit ends the relaunch.
    let w = f.client(cid).window(&surface);
    w.set_size(300, 200);
    w.attach_new_buffer();
    w.ack_last_and_commit();
    f.double_roundtrip(cid);

    assert!(
        window_by_app_id(&mut f, "myapp").is_some(),
        "the window adopted the slot"
    );
    assert!(
        !f.state().is_suspended_launching(sid),
        "adoption ended the relaunch, so a render-time lookup would read plain — \
         the fade has to have captured the launching state at creation"
    );
}

/// A client that dies without a clean unmap (crash) leaves an animation entry
/// keyed by a now-dead id; the sweep beside `retain_alive` in
/// `refresh_and_flush_clients` drains it on the next pump, and the fixture
/// baseline holds.
#[test]
fn crash_path_dead_id_sweep_drains_the_map() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    map_window(&mut f, id, "crash", (400, 300));
    assert_eq!(
        f.state().window_animations.len(),
        1,
        "the mapped window has an open entry"
    );

    // Abrupt death: no close request, no unmap_window.
    f.kill_client(id);
    f.pump(5);

    assert_eq!(
        f.state().window_animations.len(),
        0,
        "the dead-id sweep drained the animation for the crashed window"
    );
}

/// A window animation activates only the outputs its visual rect intersects: an
/// animation on output A leaves output B (a far-tiled second output) inactive.
#[test]
fn per_output_predicate_scopes_to_the_intersecting_output() {
    let mut f = Fixture::new();
    let out1 = f.add_output(1, (1920, 1080));
    let out2 = f.add_output(2, (1280, 720));
    let id = f.add_client();
    map_window(&mut f, id, "a", (400, 300));
    let window = window_by_app_id(&mut f, "a").unwrap();

    // Seat the window squarely on output 1, park output 2's camera on a far canvas
    // region so the window's rect can't reach its viewport, and quiet both cameras.
    {
        let mut os = crate::state::output_state(&out1);
        os.camera = Point::from((0.0, 0.0));
        os.zoom = 1.0;
        os.camera_target = None;
        os.zoom_target = None;
        os.zoom_animation_anchor = None;
        os.overview_return = None;
        os.edge_pan_velocity = None;
        os.momentum.stop();
    }
    {
        let mut os = crate::state::output_state(&out2);
        os.camera = Point::from((10_000.0, 0.0));
        os.zoom = 1.0;
        os.camera_target = None;
        os.zoom_target = None;
        os.zoom_animation_anchor = None;
        os.overview_return = None;
        os.edge_pan_velocity = None;
        os.momentum.stop();
    }
    f.state()
        .map_window(window.clone(), Point::from((400, 300)), false);
    f.state().update_output_from_camera();

    // The open entry from the map is live and lies on output 1 only.
    assert!(
        f.state().window_animations.is_active(),
        "an open entry is in flight"
    );
    assert!(
        f.state().output_has_active_animations(&out1),
        "output 1 shows the animation"
    );
    assert!(
        !f.state().output_has_active_animations(&out2),
        "output 2 (far tiled region) does not"
    );
    assert!(f.state().has_active_animations());

    tick_until_settled(&mut f);
}

/// An animation whose visual rect intersects no drawable output completes
/// instantly: starting one off every viewport creates no entry, and an in-flight
/// entry whose rect leaves every viewport is swept on the next tick.
#[test]
fn off_screen_animations_complete_instantly() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    map_window(&mut f, id, "a", (400, 300));
    let window = window_by_app_id(&mut f, "a").unwrap();
    reset_view(&mut f);
    f.state()
        .map_window(window.clone(), Point::from((400, 300)), false);
    tick_until_settled(&mut f);

    // Pan far so the window's canvas rect intersects no viewport, then try to
    // start an open animation — it must not start (instant-complete at start).
    f.state().set_camera(Point::from((100_000.0, 100_000.0)));
    f.state().update_output_from_camera();
    f.state().start_window_open_animation(&window);
    assert_eq!(
        f.state().window_animations.len(),
        0,
        "an off-screen animation never starts (completes instantly)"
    );

    // An in-flight entry that loses eligibility mid-flight is swept on the tick.
    reset_view(&mut f);
    f.state()
        .map_window(window.clone(), Point::from((400, 300)), false);
    let serial = smithay::utils::SERIAL_COUNTER.next_serial();
    f.state().raise_and_focus(&window, serial);
    f.state()
        .execute_action(&Action::NudgeWindow(Direction::Right));
    assert!(
        f.state().window_animations.is_active(),
        "the nudge started an entry while on-screen"
    );

    f.state().set_camera(Point::from((100_000.0, 100_000.0)));
    f.state().update_output_from_camera();
    f.state().tick_window_animations(TICK);
    assert_eq!(
        f.state().window_animations.len(),
        0,
        "the entry was completed instantly when it left every viewport"
    );
}

/// Foot-family terminals unmap (null-buffer commit) before destroying their
/// toplevel, which collapses the window's live geometry — a close animation
/// sized from `window.geometry()` at teardown got a zero-sized rect and
/// silently dropped the fade. This pins that hazard directly, since the render
/// path itself is backend-gated and can't be asserted headlessly.
#[test]
fn an_unmapped_window_no_longer_reports_its_geometry() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let surface = map_window(&mut f, id, "foot", (400, 300));
    let window = window_by_app_id(&mut f, "foot").unwrap();
    assert_eq!(
        window.geometry().size,
        Size::from((400, 300)),
        "a mapped window reports its size"
    );

    // The unmap commit, exactly as foot sequences it before destroying.
    f.client(id).window(&surface).attach_null();
    f.client(id).window(&surface).commit();
    f.roundtrip(id);
    f.dispatch();

    let live = window.geometry().size;
    assert!(
        live.w <= 0 || live.h <= 0,
        "an unmapped window reports no usable geometry (got {live:?}) — a close \
         animation must use the rect captured at unmap, not this"
    );
}

/// Unfill animates as one leg from the filled rect to the restored rect. The
/// restore position has to be applied up front: if the stage still holds the
/// fill position while only the size animates, the window shrinks anchored at
/// the fill rect's top-left and jumps to its real position only when the
/// settle fires on the client's resized commit.
#[test]
fn unfill_animates_straight_to_the_restored_rect() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let surface = map_window(&mut f, id, "a", (400, 300));
    let window = window_by_app_id(&mut f, "a").unwrap();
    reset_view(&mut f);
    f.state()
        .map_window(window.clone(), Point::from((600, 400)), false);
    let eid = element_id(&mut f, &window);
    tick_until_settled(&mut f);
    let restored_pos = f.state().stage.position_of(&window).unwrap();

    // Fill, let the client catch up, and drain the fill animation.
    f.state().fill_window(&window);
    f.double_roundtrip(id);
    super::adopt_last_configure(&mut f, id, &surface);
    f.double_roundtrip(id);
    tick_until_settled(&mut f);
    let fill_pos = f.state().stage.position_of(&window).unwrap();
    let fill_size = window.geometry().size;
    assert_ne!(fill_pos, restored_pos, "the fill moved the window");

    f.state().unfill_window(&window);

    assert_eq!(
        f.state().stage.position_of(&window),
        Some(restored_pos),
        "unfill applies the restored position immediately, so the chase has one \
         target — not the fill position with a deferred jump"
    );
    let from = f
        .state()
        .window_animations
        .geometry_visual_rect(eid)
        .expect("unfill started a geometry chase");
    assert!(
        dist(from.loc, fill_pos.to_f64()) <= 0.5,
        "the chase starts at the filled rect ({from:?} vs {fill_pos:?})"
    );

    // Every intermediate visual sits strictly between the two rects; none lands on
    // the target corner while still filled-size (the reported top-left jump).
    for _ in 0..4 {
        f.state().tick_window_animations(TICK);
        let v = f
            .state()
            .window_animations
            .geometry_visual_rect(eid)
            .expect("still in flight");
        let at_target_corner = dist(v.loc, restored_pos.to_f64()) <= 0.5;
        let still_filled = (v.size.w - fill_size.w as f64).abs() <= 0.5;
        assert!(
            !(at_target_corner && still_filled),
            "visual reached the target corner while still filled-size: {v:?}"
        );
    }
}

/// An adopted window must occupy the stand-in's slot from the first frame,
/// since the client is still committing buffers at its own mapped size until
/// it acks the resize — without a geometry hold it draws undersized beneath
/// the fading stand-in chrome, reading as a flicker rather than a crossfade.
#[test]
fn adoption_holds_the_adopted_rect_until_the_client_catches_up() {
    let tmp = TempDir::new();
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    let (_sid, cid, surface) = arrange_pending_relaunch(&mut f, &tmp);
    // The stand-in is 600x400; the returning client maps at 300x200.
    let w = f.client(cid).window(&surface);
    w.set_size(300, 200);
    w.attach_new_buffer();
    w.ack_last_and_commit();
    f.double_roundtrip(cid);

    let adopted = window_by_app_id(&mut f, "myapp").expect("the window adopted the slot");
    let eid = element_id(&mut f, &adopted);
    let pos = f.state().stage.position_of(&adopted).unwrap();

    let visual = f
        .state()
        .window_animations
        .geometry_visual_rect(eid)
        .expect("adoption seeded a geometry entry holding the slot");
    assert!(
        dist(visual.loc, pos.to_f64()) <= 0.5,
        "the held rect sits at the adopted position ({visual:?} vs {pos:?})"
    );
    assert!(
        (visual.size.w - 600.0).abs() <= 0.5 && (visual.size.h - 400.0).abs() <= 0.5,
        "the window is drawn at the stand-in's size, not its own 300x200 ({visual:?})"
    );

    // The hold survives ticks while the request is outstanding, so the mismatch
    // is never visible.
    let base = Instant::now();
    for _ in 0..30 {
        f.state().tick_window_animations_at(TICK, base);
    }
    let held = f
        .state()
        .window_animations
        .geometry_visual_rect(eid)
        .expect("still holding while the client has not acked");
    assert!(
        (held.size.w - 600.0).abs() <= 0.5,
        "still filling the slot ({held:?})"
    );

    // And the content is actually stretched to fill it. A slot hold is the one
    // case where a stale buffer must be magnified, or the adopted window
    // renders undersized at the slot's corner.
    let committed = adopted.geometry().size.to_f64();
    let v = f.state().animated_visual(eid, pos.to_f64(), committed);
    let (sx, sy) = crate::state::window_animation::content_scale(v.size, committed, v.cap_content);
    assert!(
        (sx - 600.0 / committed.w).abs() < 1e-6 && (sy - 400.0 / committed.h).abs() < 1e-6,
        "the held slot stretches the stale buffer to fill it (got {sx:.2}x, {sy:.2}x)"
    );
}

/// A compositor resize no longer stretches a stale buffer at all: the window is
/// frozen at its pre-action appearance until the client redraws. Only when the
/// client misses the budget does the leg run with stale content, and then the old
/// cap applies — the interface never balloons either way.
#[test]
fn a_growing_resize_freezes_rather_than_magnifying() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let _surface = map_window(&mut f, id, "a", (400, 300));
    let window = window_by_app_id(&mut f, "a").unwrap();
    reset_view(&mut f);
    f.state()
        .map_window(window.clone(), Point::from((600, 400)), false);
    let eid = element_id(&mut f, &window);
    tick_until_settled(&mut f);

    let seed_size = window.geometry().size.to_f64();
    f.state().fit_window(&window);
    let base = Instant::now();
    for _ in 0..30 {
        f.state().tick_window_animations_at(TICK, base);
    }

    // Frozen at the seed: same size as before the action, and drawn 1:1.
    let committed = window.geometry().size.to_f64();
    let loc = f.state().stage.position_of(&window).unwrap().to_f64();
    let v = f.state().animated_visual(eid, loc, committed);
    assert!(
        f.state().window_animations.start_held(eid),
        "the fit waits for the client's redraw"
    );
    assert!(
        (v.size.w - seed_size.w).abs() <= 0.5,
        "the rect has not grown yet ({:?})",
        v.size
    );
    assert!(
        !v.cap_content,
        "and nothing is being capped, because nothing is stretched"
    );

    // Degrade: past the budget the leg runs, and now the cap protects the stale
    // buffer from being blown up to meet the growing rect.
    let past = base + PAST_HOLD;
    for _ in 0..12 {
        f.state().tick_window_animations_at(TICK, past);
        let v = f.state().animated_visual(eid, loc, committed);
        let (sx, sy) =
            crate::state::window_animation::content_scale(v.size, committed, v.cap_content);
        assert!(
            sx <= 1.0 && sy <= 1.0,
            "the degraded leg stays capped (got {sx:.2}x)"
        );
    }
    assert!(
        f.state().animated_visual(eid, loc, committed).cap_content,
        "the degraded leg is the capped, stale-content case"
    );
}

/// The far end of a fit the client never acks: the endpoint hold's deadline
/// fires, drops the request, and the rect walks back down to the size the client
/// still has. Staleness belongs to the buffer, not to the request that release
/// just dropped, so that last leg stays capped — nothing magnifies on the way
/// back. Reaching it costs both budgets, hence the two clock steps.
#[test]
fn the_hold_deadline_release_stays_capped() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let _surface = map_window(&mut f, id, "a", (400, 300));
    let window = window_by_app_id(&mut f, "a").unwrap();
    reset_view(&mut f);
    f.state()
        .map_window(window.clone(), Point::from((600, 400)), false);
    let eid = element_id(&mut f, &window);
    tick_until_settled(&mut f);

    f.state().fit_window(&window);
    let base = Instant::now();
    f.state().tick_window_animations_at(TICK, base);

    // Past the start budget the leg degrades and runs to the requested endpoint,
    // where the endpoint hold anchors its own deadline.
    let after_start = base + PAST_HOLD;
    for _ in 0..MAX_TICKS {
        f.state().tick_window_animations_at(TICK, after_start);
        if !f.state().window_animations.start_held(eid) {
            break;
        }
    }
    for _ in 0..60 {
        f.state().tick_window_animations_at(TICK, after_start);
    }
    let endpoint = f
        .state()
        .window_animations
        .geometry_visual_rect(eid)
        .expect("the endpoint hold keeps it alive");

    // Past the endpoint budget too: the request is dropped and the rect shrinks
    // back toward the live size.
    let after_endpoint = after_start + PAST_HOLD;
    let committed = window.geometry().size.to_f64();
    let loc = f.state().stage.position_of(&window).unwrap().to_f64();
    let mut released = false;
    for _ in 0..MAX_TICKS {
        if !f.state().window_animations.is_active() {
            break;
        }
        f.state().tick_window_animations_at(TICK, after_endpoint);
        let v = f.state().animated_visual(eid, loc, committed);
        let (sx, sy) =
            crate::state::window_animation::content_scale(v.size, committed, v.cap_content);
        assert!(
            sx <= 1.0 && sy <= 1.0,
            "the release leg must stay capped — no commit ever landed (got {sx:.2}x)"
        );
        released |= v.size.w < endpoint.size.w - 1.0;
    }
    assert!(
        released,
        "the deadline dropped the request and the rect actually walked back down \
         from {endpoint:?}"
    );
    assert_eq!(
        f.state().window_animations.len(),
        0,
        "and the release leg settled at the live size"
    );
}

/// A position-only retarget (a nudge, a cluster shift) landing on a frozen resize
/// keeps the freeze — it is the same wait, just aimed somewhere else. It must not
/// cancel the wait and start animating with content the client has not
/// delivered.
#[test]
fn a_position_only_retarget_keeps_a_frozen_resize_frozen() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let _surface = map_window(&mut f, id, "a", (400, 300));
    let window = window_by_app_id(&mut f, "a").unwrap();
    reset_view(&mut f);
    f.state()
        .map_window(window.clone(), Point::from((600, 400)), false);
    let eid = element_id(&mut f, &window);
    tick_until_settled(&mut f);

    f.state().fit_window(&window);
    let base = Instant::now();
    for _ in 0..4 {
        f.state().tick_window_animations_at(TICK, base);
    }
    assert!(
        f.state().window_animations.start_held(eid),
        "frozen by the fit"
    );
    let generation = f.state().window_animations.generation_of(eid);

    // Nudge it while frozen.
    let from = f.state().stage.position_of(&window).unwrap();
    f.state()
        .map_window(window.clone(), Point::from((from.x + 40, from.y)), false);
    f.state().animate_window_move_from(&window, from, None);

    assert!(
        f.state().window_animations.start_held(eid),
        "a move does not cancel the wait for the client's redraw"
    );
    assert_eq!(
        f.state().window_animations.generation_of(eid),
        generation,
        "and does not invalidate the resize — no new request was made"
    );

    // It is still the wait it was, so it still ends when that wait's budget does.
    let past = base + crate::state::window_animation::MAX_START_HOLD + TICK;
    f.state().tick_window_animations_at(TICK, past);
    assert!(
        !f.state().window_animations.start_held(eid),
        "and the budget it was armed with still ends it"
    );
}

/// A moving freeze keeps the budget it started with. Re-arming it on every
/// position-only retarget would let a held nudge key refresh the deadline faster
/// than it expires and leave the window frozen for as long as the key is down.
#[test]
fn repeated_nudges_never_extend_a_freeze() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let _surface = map_window(&mut f, id, "a", (400, 300));
    let window = window_by_app_id(&mut f, "a").unwrap();
    reset_view(&mut f);
    f.state()
        .map_window(window.clone(), Point::from((600, 400)), false);
    let eid = element_id(&mut f, &window);
    tick_until_settled(&mut f);

    f.state().fit_window(&window);
    // The first tick anchors the deadline at `base`.
    let base = Instant::now();
    f.state().tick_window_animations_at(TICK, base);
    assert!(
        f.state().window_animations.start_held(eid),
        "frozen by the fit"
    );

    // Key repeat: a nudge every 50ms as the clock walks toward the deadline.
    for step in 1..=4 {
        let now = base + Duration::from_millis(50 * step);
        let from = f.state().stage.position_of(&window).unwrap();
        f.state()
            .map_window(window.clone(), Point::from((from.x + 40, from.y)), false);
        f.state().animate_window_move_from(&window, from, None);
        f.state().tick_window_animations_at(TICK, now);
        assert!(
            f.state().window_animations.start_held(eid),
            "still frozen before the original deadline (step {step})"
        );
    }

    // 200ms of nudging bought no extra budget.
    let past = base + crate::state::window_animation::MAX_START_HOLD + TICK;
    f.state().tick_window_animations_at(TICK, past);
    assert!(
        !f.state().window_animations.start_held(eid),
        "the freeze expired on the deadline it was armed with"
    );
}

/// The far end follows the same rule: a nudge at the endpoint moves the wait, it
/// does not re-open its budget. Otherwise a held nudge key refreshes the endpoint
/// deadline faster than it expires and parks the window on a size the client
/// never took, for as long as the key is down.
#[test]
fn repeated_nudges_never_extend_an_endpoint_hold() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let _surface = map_window(&mut f, id, "a", (400, 300));
    let window = window_by_app_id(&mut f, "a").unwrap();
    reset_view(&mut f);
    f.state()
        .map_window(window.clone(), Point::from((600, 400)), false);
    let eid = element_id(&mut f, &window);
    tick_until_settled(&mut f);

    f.state().fit_window(&window);
    let base = Instant::now();
    f.state().tick_window_animations_at(TICK, base);

    // Past the start budget the leg degrades and runs to the requested endpoint,
    // where the endpoint hold anchors its deadline.
    let parked = base + PAST_HOLD;
    for _ in 0..MAX_TICKS {
        f.state().tick_window_animations_at(TICK, parked);
        if !f.state().window_animations.start_held(eid) {
            break;
        }
    }
    for _ in 0..60 {
        f.state().tick_window_animations_at(TICK, parked);
    }
    let endpoint = f
        .state()
        .window_animations
        .geometry_visual_rect(eid)
        .expect("the endpoint hold keeps it alive");

    // Key repeat: a nudge every 100ms, each re-converging on the requested rect.
    for step in 1..=4 {
        let now = parked + Duration::from_millis(100 * step);
        let from = f.state().stage.position_of(&window).unwrap();
        f.state()
            .map_window(window.clone(), Point::from((from.x + 40, from.y)), false);
        f.state().animate_window_move_from(&window, from, None);
        for _ in 0..20 {
            f.state().tick_window_animations_at(TICK, now);
        }
        let v = f
            .state()
            .window_animations
            .geometry_visual_rect(eid)
            .expect("still waiting on the request");
        assert!(
            (v.size.w - endpoint.size.w).abs() <= 0.5,
            "still holding the requested size before the original deadline \
             (step {step}, {v:?})"
        );
    }

    // 400ms of nudging bought no extra budget: the request is dropped on the
    // deadline the endpoint hold anchored, and the rect walks back down.
    let past = parked + crate::state::window_animation::MAX_ENDPOINT_HOLD + TICK;
    f.state().tick_window_animations_at(TICK, past);
    f.state().tick_window_animations_at(TICK, past);
    let released = f
        .state()
        .window_animations
        .geometry_visual_rect(eid)
        .expect("the release leg is still in flight");
    assert!(
        released.size.w < endpoint.size.w - 1.0,
        "the endpoint budget expired on schedule ({endpoint:?} -> {released:?})"
    );
}

/// Sliding an adopted window keeps its slot hold: the same hold, moving. Taking
/// the mover's policy would flip it to capped and snap the content down to the
/// client's own size mid-slide.
#[test]
fn a_position_only_retarget_keeps_an_adopted_slot_stretching() {
    let tmp = TempDir::new();
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    let (_sid, cid, surface) = arrange_pending_relaunch(&mut f, &tmp);
    let w = f.client(cid).window(&surface);
    w.set_size(300, 200);
    w.attach_new_buffer();
    w.ack_last_and_commit();
    f.double_roundtrip(cid);

    let adopted = window_by_app_id(&mut f, "myapp").expect("adopted the slot");
    let eid = element_id(&mut f, &adopted);
    let from = f.state().stage.position_of(&adopted).unwrap();

    // Slide it while the slot hold is still outstanding.
    f.state()
        .map_window(adopted.clone(), Point::from((from.x + 40, from.y)), false);
    f.state().animate_window_move_from(&adopted, from, None);

    let committed = adopted.geometry().size.to_f64();
    let loc = f.state().stage.position_of(&adopted).unwrap().to_f64();
    let v = f.state().animated_visual(eid, loc, committed);
    assert!(
        !v.cap_content,
        "an adopted slot keeps stretching while it slides"
    );
    // And it keeps filling it as the slide plays out — dropping the hold would
    // bend the rect down to the client's own size instead.
    let base = Instant::now();
    for _ in 0..8 {
        f.state().tick_window_animations_at(TICK, base);
        let v = f
            .state()
            .window_animations
            .geometry_visual_rect(eid)
            .expect("the hold is still in flight");
        assert!(
            (v.size.w - 600.0).abs() <= 0.5 && (v.size.h - 400.0).abs() <= 0.5,
            "the slot rect survives the slide ({v:?})"
        );
    }
}

/// A commit arriving after both budgets have run out — the request already gone,
/// dropped by the endpoint release rather than by any commit — is still the
/// resolution: it clears staleness, so the flag never outlives the buffer it
/// describes. The arm under test only exists once nothing is outstanding, which
/// is why the request has to be genuinely dropped first.
#[test]
fn a_late_commit_after_the_deadline_clears_staleness() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let surface = map_window(&mut f, id, "a", (400, 300));
    let window = window_by_app_id(&mut f, "a").unwrap();
    reset_view(&mut f);
    f.state()
        .map_window(window.clone(), Point::from((600, 400)), false);
    let eid = element_id(&mut f, &window);
    tick_until_settled(&mut f);

    f.state().fit_window(&window);
    let base = Instant::now();
    f.state().tick_window_animations_at(TICK, base);

    // Past the start budget the leg degrades and parks at the requested endpoint.
    let after_start = base + PAST_HOLD;
    for _ in 0..MAX_TICKS {
        f.state().tick_window_animations_at(TICK, after_start);
        if !f.state().window_animations.start_held(eid) {
            break;
        }
    }
    for _ in 0..60 {
        f.state().tick_window_animations_at(TICK, after_start);
    }
    let endpoint = f
        .state()
        .window_animations
        .geometry_visual_rect(eid)
        .expect("the endpoint hold keeps it alive");

    // Past the endpoint budget the request is dropped — still with no commit —
    // and the rect starts back toward the size the client actually has.
    let after_endpoint = after_start + PAST_HOLD;
    f.state().tick_window_animations_at(TICK, after_endpoint);
    f.state().tick_window_animations_at(TICK, after_endpoint);
    let committed = window.geometry().size.to_f64();
    let loc = f.state().stage.position_of(&window).unwrap().to_f64();
    let released = f
        .state()
        .window_animations
        .geometry_visual_rect(eid)
        .unwrap();
    assert!(
        released.size.w < endpoint.size.w - 1.0,
        "the deadline dropped the request, so nothing is left for the commit \
         below to resolve ({endpoint:?} -> {released:?})"
    );
    assert!(
        f.state().animated_visual(eid, loc, committed).cap_content,
        "still stale right after the release — no commit yet"
    );

    // The client finally redraws, at a size of its own.
    let w = f.client(id).window(&surface);
    w.set_size(900, 700);
    w.attach_new_buffer();
    w.ack_last_and_commit();
    f.double_roundtrip(id);

    assert!(
        f.state().window_animations.is_active(),
        "the release leg is still running for the late commit to land on"
    );
    let committed = window.geometry().size.to_f64();
    assert!(
        !f.state().animated_visual(eid, loc, committed).cap_content,
        "the late commit is the resolution arriving, so staleness is cleared"
    );
}

// Dismissing a focused stand-in follows the same tiers a real close does: the
// spatially-related history entry first (panning to it only when
// `auto_navigate_on_close` allows), else a visible window on the stand-in's home
// output — never panning in that arm.

/// Helper: a stand-in at `pos` plus a live client window at `win_pos`, camera at
/// the origin with animations quiet. Returns the client's window handle.
fn standin_and_window(
    f: &mut Fixture,
    pos: Point<i32, Logical>,
    win_pos: Point<i32, Logical>,
) -> (crate::state::SuspendedId, smithay::desktop::Window) {
    f.add_output(1, (1920, 1080));
    f.skip_baseline_check();
    let id = f.add_client();
    map_window(f, id, "live", (400, 300));
    let window = window_by_app_id(f, "live").unwrap();
    f.state().with_output_state(|os| {
        os.camera = Point::from((0.0, 0.0));
        os.zoom = 1.0;
        os.camera_target = None;
        os.zoom_target = None;
        os.zoom_animation_anchor = None;
        os.overview_return = None;
        os.momentum.stop();
    });
    f.state().map_window(window.clone(), win_pos, false);
    f.state().update_output_from_camera();
    let sid = f
        .state()
        .insert_suspended_for_test(1, pos, Size::from((400, 300)), "myapp", "myapp");
    f.state()
        .set_suspended_focus(sid, smithay::utils::SERIAL_COUNTER.next_serial());
    (sid, window)
}

/// (a) A focused stand-in clustered with a window that is scrolled off-screen:
/// with auto-navigation on, the dismiss pans to it, exactly as a close would.
#[test]
fn dismissing_a_focused_stand_in_navigates_to_a_related_off_screen_window() {
    let mut f = Fixture::new();
    // The window sits immediately right of the stand-in (snapped: same cluster),
    // and both are far from the camera so the follow target is off-screen.
    let (sid, _window) =
        standin_and_window(&mut f, Point::from((6000, 600)), Point::from((6412, 600)));

    f.state().dismiss_suspended(sid);

    assert!(
        f.state().camera_target().is_some(),
        "a related off-screen follow target pans, like a close does"
    );
}

/// (b) No spatial relation and the only MRU window is off-screen: the dismiss
/// must not pan. Focus falls to a visible window on the home output, or clears.
#[test]
fn dismissing_an_unrelated_focused_stand_in_does_not_pan() {
    let mut f = Fixture::new();
    // The window is nowhere near the stand-in, and off the stand-in's viewport.
    let (sid, _window) =
        standin_and_window(&mut f, Point::from((300, 300)), Point::from((40000, 40000)));
    let before = f.state().camera();

    f.state().dismiss_suspended(sid);

    assert_eq!(f.state().camera(), before, "the no-follow arm never pans");
    assert!(
        f.state().camera_target().is_none(),
        "and arms no camera animation"
    );
}

/// (c) Same shape as (a) but with auto-navigation off: the off-screen follow is
/// dropped rather than panned to.
#[test]
fn dismissing_with_auto_navigate_off_drops_an_off_screen_follow() {
    let mut f = Fixture::with_config(
        Config::from_toml("[navigation]\nauto_navigate_on_close = false\n").unwrap(),
    );
    let (sid, _window) =
        standin_and_window(&mut f, Point::from((6000, 600)), Point::from((6412, 600)));
    let before = f.state().camera();

    f.state().dismiss_suspended(sid);

    assert_eq!(
        f.state().camera(),
        before,
        "auto_navigate_on_close = false never pans on dismiss"
    );
    assert!(f.state().camera_target().is_none());
}

/// (d) Dismissing a stand-in that never held focus leaves focus and camera alone.
#[test]
fn dismissing_an_unfocused_stand_in_changes_nothing() {
    let mut f = Fixture::new();
    let (sid, window) =
        standin_and_window(&mut f, Point::from((6000, 600)), Point::from((6412, 600)));
    // Hand focus to the live window, so the stand-in is not the focused element.
    let serial = smithay::utils::SERIAL_COUNTER.next_serial();
    f.state().raise_and_focus(&window, serial);
    let before = f.state().camera();

    f.state().dismiss_suspended(sid);

    assert_eq!(f.state().camera(), before, "no camera change");
    assert!(f.state().camera_target().is_none());
    assert!(
        f.state().focused_window().is_some(),
        "the live window keeps focus"
    );
}

/// A resize freeze renders the window exactly as it looked before the action —
/// which for a frame-converted seed (entering fullscreen from a zoomed-in canvas)
/// is not 1:1. Capping the content there would visibly shrink the "frozen"
/// window, so the hold is deliberately uncapped at whatever the seed ratio is.
#[test]
fn a_frozen_resize_renders_uncapped_at_its_seed_ratio() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let _surface = map_window(&mut f, id, "a", (400, 300));
    let window = window_by_app_id(&mut f, "a").unwrap();
    reset_view(&mut f);
    let eid = element_id(&mut f, &window);
    tick_until_settled(&mut f);

    // A seed twice the committed size, as a fullscreen enter at zoom 2 produces.
    let committed = window.geometry().size.to_f64();
    let loc = f.state().stage.position_of(&window).unwrap().to_f64();
    let seed = Rectangle::new(loc, Size::from((committed.w * 2.0, committed.h * 2.0)));
    f.state().begin_geometry_animation_seeded(
        &window,
        seed,
        crate::state::window_animation::AnimSpace::Canvas,
        Some(Size::from((1896, 1056))),
        crate::state::window_animation::GeometryRole::FullscreenEntry { was_pinned: false },
        crate::state::window_animation::ContentPolicy::Cap,
        None,
    );
    let base = Instant::now();
    for _ in 0..10 {
        f.state().tick_window_animations_at(TICK, base);
    }

    assert!(f.state().window_animations.start_held(eid), "frozen");
    let v = f.state().animated_visual(eid, loc, committed);
    assert!(!v.cap_content, "a frozen window is never capped");
    let (sx, sy) = crate::state::window_animation::content_scale(v.size, committed, v.cap_content);
    assert!(
        (sx - 2.0).abs() < 1e-6 && (sy - 2.0).abs() < 1e-6,
        "it renders at the seed ratio, reproducing the pre-action look ({sx:.2}x)"
    );
}

/// A fullscreen enter flips stage membership at the action, but the freeze holds
/// the *windowed* picture on screen for the length of its budget after that.
/// Chrome follows the picture, not the membership — stripping the bar, border and
/// shadow (and uncropping a CSD client's own shadow) at the action would leave a
/// motionless frame wearing the wrong dress for the whole freeze. The client's
/// redraw then starts the exchange rather than finishing it: the chrome fades out
/// across the grow instead of blinking off while the window is still small.
#[test]
fn a_frozen_fullscreen_enter_keeps_its_windowed_chrome() {
    let mut f = Fixture::new();
    let output = f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let surface = map_window(&mut f, id, "fs", (800, 600));
    let window = window_by_app_id(&mut f, "fs").unwrap();
    reset_view(&mut f);
    let eid = element_id(&mut f, &window);
    tick_until_settled(&mut f);

    f.client(id).window(&surface).set_fullscreen(None);
    f.double_roundtrip(id);
    assert!(
        f.state().window_animations.start_held(eid),
        "the enter waits for the client to redraw at the fullscreen size"
    );
    assert!(
        f.state().stage.is_fullscreen(&window),
        "the stage flipped the instant the action ran"
    );
    assert_eq!(
        chrome_alpha(&mut f, &window),
        1.0,
        "but the picture on screen is still the windowed one, chrome and all"
    );

    // The redraw the freeze was waiting for. The window starts growing here, and
    // the chrome starts leaving with it — neither is done yet.
    super::adopt_last_configure(&mut f, id, &surface);
    assert_eq!(
        chrome_alpha(&mut f, &window),
        1.0,
        "the leg has not travelled yet, so the chrome is all still there"
    );
    f.state().tick_window_animations(TICK);
    let mid = chrome_alpha(&mut f, &window);
    assert!(
        mid > 0.0 && mid < 1.0,
        "it hands over across the leg rather than at one frame ({mid})"
    );
    tick_until_settled(&mut f);
    assert!(
        f.state().chrome_fullscreen(&window),
        "and is gone once the window fills the output"
    );

    f.state().exit_fullscreen_on(&output);
}

/// The mirror case, and the one a user can nudge: an exit's freeze holds the
/// fullscreen picture after the stage has already let it go, so chrome stays off
/// until the client redraws at its windowed size. A position-only retarget is the
/// same freeze moving and must not restate what that picture wore.
#[test]
fn a_frozen_fullscreen_exit_keeps_its_fullscreen_chrome() {
    let mut f = Fixture::new();
    let output = f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let surface = map_window(&mut f, id, "fs", (800, 600));
    let window = window_by_app_id(&mut f, "fs").unwrap();
    reset_view(&mut f);
    let eid = element_id(&mut f, &window);
    f.client(id).window(&surface).set_fullscreen(None);
    f.double_roundtrip(id);
    super::adopt_last_configure(&mut f, id, &surface);
    tick_until_settled(&mut f);

    f.state().exit_fullscreen_on(&output);
    f.double_roundtrip(id);
    assert!(
        f.state().window_animations.start_held(eid),
        "the exit waits for the client to redraw at its windowed size"
    );
    assert!(
        !f.state().stage.is_fullscreen(&window),
        "the stage let it go the instant the action ran"
    );
    assert_eq!(
        chrome_alpha(&mut f, &window),
        0.0,
        "but the picture on screen is still the fullscreen one, so no chrome"
    );

    let from = f.state().stage.position_of(&window).unwrap();
    f.state()
        .map_window(window.clone(), Point::from((from.x + 40, from.y)), false);
    f.state().animate_window_move_from(&window, from, None);
    assert_eq!(
        chrome_alpha(&mut f, &window),
        0.0,
        "a nudge moves the freeze, it does not redress the frozen picture"
    );

    super::adopt_last_configure(&mut f, id, &surface);
    f.state().tick_window_animations(TICK);
    let mid = chrome_alpha(&mut f, &window);
    assert!(
        mid > 0.0 && mid < 1.0,
        "the windowed redraw brings the chrome back across the shrink ({mid})"
    );
    tick_until_settled(&mut f);
    assert_eq!(
        chrome_alpha(&mut f, &window),
        1.0,
        "and it is fully there once the window is back"
    );
}

/// The chrome hand-over keeps its own clock. A position-only retarget starts a
/// fresh leg — a full-duration slide from wherever the window is — while
/// deliberately leaving the picture that leg started from alone, so chrome that
/// is already half back must not dip and fade in a second time.
#[test]
fn a_nudge_mid_shrink_does_not_re_fade_the_chrome() {
    let mut f = Fixture::new();
    let output = f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let surface = map_window(&mut f, id, "fs", (800, 600));
    let window = window_by_app_id(&mut f, "fs").unwrap();
    reset_view(&mut f);
    f.client(id).window(&surface).set_fullscreen(None);
    f.double_roundtrip(id);
    super::adopt_last_configure(&mut f, id, &surface);
    tick_until_settled(&mut f);

    f.state().exit_fullscreen_on(&output);
    f.double_roundtrip(id);
    super::adopt_last_configure(&mut f, id, &surface);
    f.state().tick_window_animations(TICK);
    let before = chrome_alpha(&mut f, &window);
    assert!(
        before > 0.0 && before < 1.0,
        "precondition: the shrink is partway through bringing the chrome back \
         ({before})"
    );

    let from = f.state().stage.position_of(&window).unwrap();
    f.state()
        .map_window(window.clone(), Point::from((from.x + 100, from.y)), false);
    f.state().animate_window_move_from(&window, from, None);
    let after = chrome_alpha(&mut f, &window);
    assert!(
        after >= before,
        "the nudge is the same picture moving, not a second hand-over: the \
         chrome fell back from {before} to {after}"
    );

    f.state().tick_window_animations(TICK);
    let next = chrome_alpha(&mut f, &window);
    assert!(next >= after, "and it keeps arriving ({after} -> {next})");
    tick_until_settled(&mut f);
    assert_eq!(chrome_alpha(&mut f, &window), 1.0);
}

/// The endpoint hold expiring re-seeds the leg toward the live size — with no
/// user input at all, on a client that simply never redrew. The picture the
/// hand-over started from has not changed, so the windowed chrome must not
/// reappear over a window that has already grown to fill the output.
#[test]
fn an_expired_endpoint_hold_does_not_bring_the_chrome_back() {
    let mut f = Fixture::new();
    let output = f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let surface = map_window(&mut f, id, "fs", (400, 300));
    let window = window_by_app_id(&mut f, "fs").unwrap();
    reset_view(&mut f);
    let eid = element_id(&mut f, &window);
    tick_until_settled(&mut f);

    f.client(id).window(&surface).set_fullscreen(None);
    f.double_roundtrip(id);
    let base = Instant::now();
    f.state().tick_window_animations_at(TICK, base);

    // Nothing acks: the freeze degrades and the leg runs to the fullscreen
    // endpoint on stale content, with the request still outstanding.
    let degraded = base + Duration::from_millis(400);
    for _ in 0..40 {
        f.state().tick_window_animations_at(TICK, degraded);
    }
    assert!(
        !f.state().window_animations.start_held(eid),
        "the start budget expired"
    );
    assert!(
        f.state().chrome_fullscreen(&window),
        "the leg reached the output bounds and the chrome went with it"
    );

    // The endpoint budget expires in turn, dropping the request and re-seeding
    // the leg back toward the size the client never left.
    let past = degraded + PAST_HOLD;
    for _ in 0..3 {
        f.state().tick_window_animations_at(TICK, past);
        assert!(
            f.state().chrome_fullscreen(&window),
            "a re-seeded leg is not a new picture: chrome came back at {}",
            chrome_alpha(&mut f, &window)
        );
    }

    f.state().exit_fullscreen_on(&output);
}

/// A *resize* landing on a frozen exit is a new request, so it re-freezes and
/// re-arms — but the picture on screen is still the fullscreen one it froze on,
/// so the chrome stamp has to survive. Restating it from the fit's role would pop
/// a bar, border and shadow onto a motionless fullscreen frame.
#[test]
fn a_fit_during_a_fullscreen_exit_freeze_keeps_the_frozen_chrome() {
    // The fit has to be a real request: at the default zero outer gap it would
    // land on the very rect the fullscreen already configured and change nothing.
    let mut f = Fixture::with_config(Config::from_toml("[snap]\nouter_gap = 12.0\n").unwrap());
    let output = f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let surface = map_window(&mut f, id, "fs", (800, 600));
    let window = window_by_app_id(&mut f, "fs").unwrap();
    reset_view(&mut f);
    let eid = element_id(&mut f, &window);
    f.client(id).window(&surface).set_fullscreen(None);
    f.double_roundtrip(id);
    super::adopt_last_configure(&mut f, id, &surface);
    tick_until_settled(&mut f);

    f.state().exit_fullscreen_on(&output);
    f.double_roundtrip(id);
    let generation = f.state().window_animations.generation_of(eid);
    assert_eq!(
        chrome_alpha(&mut f, &window),
        0.0,
        "the exit froze the fullscreen picture, which wears no chrome"
    );

    // A fit while that frame is still up: a genuinely new request, so the freeze
    // re-arms — but not on a new picture.
    f.state().fit_window(&window);
    assert!(
        f.state().window_animations.generation_of(eid) > generation,
        "the fit superseded the exit's request"
    );
    assert!(
        f.state().window_animations.start_held(eid),
        "and waits for the client's redraw in turn"
    );
    assert_eq!(
        chrome_alpha(&mut f, &window),
        0.0,
        "the picture it is waiting on is still the fullscreen one"
    );

    // Only the client's redraw changes it, and then only gradually — the fit's
    // leg is where the chrome the frozen picture never had arrives.
    super::adopt_last_configure(&mut f, id, &surface);
    f.state().tick_window_animations(TICK);
    let mid = chrome_alpha(&mut f, &window);
    assert!(mid > 0.0 && mid < 1.0, "the chrome fades in ({mid})");
    tick_until_settled(&mut f);
    assert_eq!(chrome_alpha(&mut f, &window), 1.0);
}

/// A fullscreen exit lets go of stage membership at the action, but for the
/// length of its freeze the picture covering the output has not moved. The output
/// has to keep reporting itself covered until the client's redraw lands —
/// otherwise the panels, the canvas background and every other window pop back in
/// over a motionless fullscreen frame.
#[test]
fn a_frozen_fullscreen_exit_still_covers_its_output() {
    let mut f = Fixture::new();
    let output = f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let surface = map_window(&mut f, id, "fs", (800, 600));
    let window = window_by_app_id(&mut f, "fs").unwrap();
    reset_view(&mut f);
    let eid = element_id(&mut f, &window);
    f.client(id).window(&surface).set_fullscreen(None);
    f.double_roundtrip(id);
    super::adopt_last_configure(&mut f, id, &surface);
    tick_until_settled(&mut f);
    assert!(f.state().is_output_visually_fullscreen(&output));

    f.state().exit_fullscreen_on(&output);
    f.double_roundtrip(id);
    assert!(
        f.state().window_animations.start_held(eid),
        "the exit waits for the client to redraw at its windowed size"
    );
    assert!(
        !f.state().is_output_fullscreen(&output),
        "the stage let go the instant the action ran"
    );
    assert!(
        f.state().is_output_visually_fullscreen(&output),
        "but the picture covering the output has not moved yet"
    );
    assert_eq!(
        f.state().visually_fullscreen_windows_on(&output),
        vec![window.clone()],
        "and the window drawing it is the one on its way out"
    );

    super::adopt_last_configure(&mut f, id, &surface);
    assert!(
        !f.state().is_output_visually_fullscreen(&output),
        "the redraw is windowed, so the output is uncovered from that frame on"
    );
    tick_until_settled(&mut f);
}

/// Handing one output's fullscreen from one window to another arms both halves
/// at once: the outgoing window's exit freezes on its fullscreen picture while
/// the incoming one grows into it. The output stays covered — nothing may pop in
/// under a motionless fullscreen frame — but its cull has to keep both pictures,
/// or the incoming window is composed out of every frame of its own growth and
/// pops in when the freeze drops.
#[test]
fn a_fullscreen_handover_draws_both_the_frozen_exit_and_the_growing_entry() {
    let mut f = Fixture::new();
    let output = f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let leaving = map_window(&mut f, id, "a", (800, 600));
    let arriving = map_window(&mut f, id, "b", (800, 600));
    let first = window_by_app_id(&mut f, "a").unwrap();
    let second = window_by_app_id(&mut f, "b").unwrap();
    reset_view(&mut f);
    tick_until_settled(&mut f);

    f.client(id).window(&leaving).set_fullscreen(None);
    f.double_roundtrip(id);
    super::adopt_last_configure(&mut f, id, &leaving);
    tick_until_settled(&mut f);
    assert!(f.state().is_output_visually_fullscreen(&output));

    f.client(id).window(&arriving).set_fullscreen(None);
    f.double_roundtrip(id);
    let leaving_id = element_id(&mut f, &first);
    let arriving_id = element_id(&mut f, &second);
    assert_eq!(
        f.state().frozen_fullscreen_cover(&output),
        Some(leaving_id),
        "precondition: the displaced window's exit is frozen on its fullscreen \
         picture, and that picture is still covering the output"
    );
    assert!(
        f.state().is_output_visually_fullscreen(&output),
        "so the output stays covered — the panels must not come back over it"
    );
    let shown = f.state().visually_fullscreen_windows_on(&output);
    assert!(
        shown.contains(&first) && shown.contains(&second),
        "but both draw: the frozen picture and the window growing into it, got \
         {} of 2",
        shown.len()
    );

    // The entry lands well before the exit's 300ms budget does, and the cover
    // reasserts alone for the rest of it — the incoming window must not vanish.
    // `now` is pinned so the outgoing client's silence can't expire its freeze.
    super::adopt_last_configure(&mut f, id, &arriving);
    let base = Instant::now();
    for _ in 0..30 {
        f.state().tick_window_animations_at(TICK, base);
    }
    assert!(
        !f.state()
            .window_animations
            .fullscreen_entry_active(arriving_id),
        "precondition: the incoming window's growth has landed"
    );
    assert_eq!(
        f.state().frozen_fullscreen_cover(&output),
        Some(leaving_id),
        "precondition: the exit is still frozen after the entry landed"
    );
    assert!(
        f.state()
            .visually_fullscreen_windows_on(&output)
            .contains(&second),
        "the window that now owns the output's fullscreen still draws over the \
         frame on its way out"
    );

    f.state().exit_fullscreen_on(&output);
}

/// The handover from a *zoomed-out* canvas. Entering fullscreen parks the live
/// viewport a whole transition ahead of what is on screen, so a claim judged
/// against the live one is judged in the wrong frame: the outgoing picture has
/// not moved, and dropping its claim lets the panels and the background back in
/// over a motionless fullscreen frame for the whole of the incoming window's
/// growth.
#[test]
fn a_fullscreen_handover_keeps_its_cover_across_the_viewport_park() {
    let mut f = Fixture::new();
    let output = f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let leaving = map_window(&mut f, id, "a", (800, 600));
    let arriving = map_window(&mut f, id, "b", (800, 600));
    let first = window_by_app_id(&mut f, "a").unwrap();
    reset_view(&mut f);
    f.state().with_output_state(|os| {
        os.camera = Point::from((37.5, 21.25));
        os.zoom = 0.8;
    });
    f.state().update_output_from_camera();
    tick_until_settled(&mut f);

    f.client(id).window(&leaving).set_fullscreen(None);
    f.double_roundtrip(id);
    super::adopt_last_configure(&mut f, id, &leaving);
    tick_until_settled(&mut f);

    f.client(id).window(&arriving).set_fullscreen(None);
    f.double_roundtrip(id);
    let leaving_id = element_id(&mut f, &first);
    assert_ne!(
        f.state().with_output_state(|os| (os.camera, os.zoom)),
        Some((Point::from((37.5, 21.25)), 0.8)),
        "precondition: the incoming entry parked the live viewport away from the \
         view the outgoing picture was frozen under"
    );
    assert_eq!(
        f.state().frozen_fullscreen_cover(&output),
        Some(leaving_id),
        "but that picture is drawn through the parked-away view, and is still \
         hiding the output"
    );
    assert!(f.state().is_output_visually_fullscreen(&output));

    f.state().exit_fullscreen_on(&output);
}

/// The z-order half of the same handover. The outgoing window re-pins on its way
/// out, so its frozen picture claims the screen-pinned bucket that draws above
/// every normal window — including the one growing into the fullscreen it is
/// handing over. On a covered output the fullscreen pictures share one bucket
/// instead, and nothing may reorder the two behind the compositor's back: a
/// pinned window's per-camera-move re-anchor must not raise it either.
#[test]
fn a_fullscreen_handover_from_a_pinned_window_does_not_bury_the_incoming_one() {
    let mut f = Fixture::with_config(
        Config::from_toml("[[window_rules]]\napp_id = \"p\"\npinned_to_screen = true\n").unwrap(),
    );
    let output = f.add_output(1, (1920, 1080));
    let other = f.add_output(2, (1280, 720));
    let id = f.add_client();
    let leaving = map_window(&mut f, id, "p", (400, 300));
    let arriving = map_window(&mut f, id, "b", (800, 600));
    let first = window_by_app_id(&mut f, "p").unwrap();
    let second = window_by_app_id(&mut f, "b").unwrap();
    reset_view(&mut f);
    tick_until_settled(&mut f);

    f.state().enter_fullscreen(&first, Some(output.clone()));
    f.double_roundtrip(id);
    super::adopt_last_configure(&mut f, id, &leaving);
    tick_until_settled(&mut f);

    f.client(id).window(&arriving).set_fullscreen(None);
    f.double_roundtrip(id);
    let leaving_id = f.state().stage.id_of(&first);
    assert!(
        f.state().pinned_picture_of(leaving_id, &first),
        "precondition: the exit re-pinned, so its frozen picture wears the pin"
    );
    let covered = f.state().is_output_visually_fullscreen(&output);
    assert!(
        covered,
        "precondition: the frozen picture still covers the output"
    );
    assert!(
        !f.state().draws_pinned_on(leaving_id, &first, covered),
        "the outgoing picture gives up the bucket that would draw it over the \
         window taking its fullscreen"
    );

    // A pan on the *other* monitor, mid-handover — momentum keeps these coming
    // for as long as the freeze lasts, and each one re-anchors every pinned
    // window on the canvas.
    crate::state::output_state(&other).camera.x += 60.0;
    f.state().update_output_from_camera();
    assert!(
        stage_index(&mut f, &second) > stage_index(&mut f, &first),
        "and stays below it in the stage's own z-order, so the shared bucket \
         still draws the incoming window on top"
    );
    assert!(
        f.state()
            .visually_fullscreen_windows_on(&output)
            .contains(&second),
        "and the incoming window survives the cull"
    );

    f.state().exit_fullscreen_on(&output);
}

/// Where `window` sits in the stage's z-order: higher is closer to the front.
fn stage_index(f: &mut Fixture, window: &Window) -> usize {
    f.state()
        .stage
        .windows()
        .position(|w| w.client() == Some(window))
        .expect("the window is stage-mapped")
}

/// A frozen fullscreen picture covers its output only while it is still drawn
/// where it was frozen. Re-entering fullscreen inside the exit's freeze reseeds
/// the rect that picture was drawn at, so the claim has to go with it —
/// otherwise the output keeps culling the whole scene while the window it names
/// is a small one growing from the corner, and the uncovered band renders black.
/// Driven pinned, where the exit's cover matches under any camera and no pan can
/// expire it.
#[test]
fn a_re_entered_fullscreen_drops_the_exit_freeze_cover() {
    let mut f = Fixture::with_config(
        Config::from_toml("[[window_rules]]\napp_id = \"p\"\npinned_to_screen = true\n").unwrap(),
    );
    let output = f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let surface = map_window(&mut f, id, "p", (400, 300));
    let window = window_by_app_id(&mut f, "p").unwrap();
    reset_view(&mut f);
    let eid = element_id(&mut f, &window);
    tick_until_settled(&mut f);

    f.state().enter_fullscreen(&window, Some(output.clone()));
    f.double_roundtrip(id);
    super::adopt_last_configure(&mut f, id, &surface);
    tick_until_settled(&mut f);

    // Exit, then straight back in — before the client has drawn anything at its
    // windowed size, so the exit is still frozen on the fullscreen picture.
    f.state().exit_fullscreen_on(&output);
    assert_eq!(
        f.state().frozen_fullscreen_cover(&output),
        Some(eid),
        "the exit's freeze holds the fullscreen picture across the output"
    );

    f.state().enter_fullscreen(&window, Some(output.clone()));
    assert!(
        f.state().window_animations.start_held(eid),
        "precondition: nothing released the freeze, so only the reseed can drop \
         the cover"
    );
    assert_eq!(
        f.state().frozen_fullscreen_cover(&output),
        None,
        "the picture that covered the output has been reseeded to the windowed \
         rect, so the scene behind it must draw"
    );
    assert_eq!(
        chrome_alpha(&mut f, &window),
        0.0,
        "but it is still the bare fullscreen picture it was frozen as — a bar, a \
         border and a shadow must not pop onto a frame that has not moved"
    );

    f.state().exit_fullscreen_on(&output);
}

/// The cross-output form, which no keybinding guard covers: a client already
/// fullscreen on one monitor asks for fullscreen on another. The enter tears the
/// first one down, arming an exit cover on the same element, then reseeds that
/// element onto the second monitor. The first monitor now names a window that
/// renders nothing there at all, so holding the cover leaves it black.
#[test]
fn a_cross_output_fullscreen_drops_the_cover_on_the_output_it_left() {
    let mut f = Fixture::new();
    let first = f.add_output(1, (1920, 1080));
    let second = f.add_output(2, (1920, 1080));
    let id = f.add_client();
    let surface = map_window(&mut f, id, "fs", (800, 600));
    let window = window_by_app_id(&mut f, "fs").unwrap();
    reset_view(&mut f);
    let eid = element_id(&mut f, &window);
    tick_until_settled(&mut f);

    f.state().enter_fullscreen(&window, Some(first.clone()));
    f.double_roundtrip(id);
    super::adopt_last_configure(&mut f, id, &surface);
    tick_until_settled(&mut f);
    assert!(f.state().is_output_visually_fullscreen(&first));

    f.state().enter_fullscreen(&window, Some(second.clone()));
    assert!(
        f.state().window_animations.start_held(eid),
        "precondition: nothing released the freeze the teardown armed"
    );
    assert_eq!(
        f.state().stage.fullscreen_output_of(&window),
        Some(second.name().as_str()),
        "precondition: the window is fullscreen on the other monitor now"
    );
    assert_eq!(
        f.state().frozen_fullscreen_cover(&first),
        None,
        "so the monitor it left is covered by nothing and must draw its scene"
    );
    assert!(!f.state().is_output_visually_fullscreen(&first));

    f.state().exit_fullscreen_on(&second);
}

/// Entering fullscreen unpins at the action, but the freeze then holds the
/// *pinned* picture on screen. Reading pin membership live restacks a frame that
/// is not moving: it drops out of the bucket that draws above every normal
/// window, and its title bar loses the pin marker mid-freeze.
#[test]
fn a_frozen_fullscreen_enter_keeps_its_pinned_bucket() {
    let mut f = Fixture::with_config(
        Config::from_toml("[[window_rules]]\napp_id = \"p\"\npinned_to_screen = true\n").unwrap(),
    );
    let output = f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let surface = map_window(&mut f, id, "p", (400, 300));
    let window = window_by_app_id(&mut f, "p").unwrap();
    reset_view(&mut f);
    let eid = element_id(&mut f, &window);
    assert!(f.state().is_pinned(&window), "the window pinned via rule");
    tick_until_settled(&mut f);

    f.state().enter_fullscreen(&window, Some(output.clone()));
    f.double_roundtrip(id);
    assert!(
        f.state().window_animations.start_held(eid),
        "the enter waits for the client to redraw fullscreen"
    );
    assert!(
        !f.state().is_pinned(&window),
        "the stage unpinned the instant the action ran"
    );
    assert!(
        f.state().pinned_picture_of(Some(eid), &window),
        "but the picture on screen is still the pinned one"
    );

    super::adopt_last_configure(&mut f, id, &surface);
    assert!(
        !f.state().pinned_picture_of(Some(eid), &window),
        "the fullscreen redraw is not, and takes the bucket with it"
    );

    f.state().exit_fullscreen_on(&output);
}

/// A compositor resize can move the window as well as resize it, and the freeze
/// holds the old picture in the old place for its whole budget. Culling on the
/// window's live rect alone then composes it out of the very frames its own
/// animation asked for — it vanishes outright, mid-flight, and reappears at the
/// destination.
#[test]
fn a_frozen_resize_that_moves_off_screen_is_still_drawn() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let _surface = map_window(&mut f, id, "a", (400, 300));
    let window = window_by_app_id(&mut f, "a").unwrap();
    reset_view(&mut f);
    f.state()
        .map_window(window.clone(), Point::from((200, 200)), false);
    let eid = element_id(&mut f, &window);
    tick_until_settled(&mut f);

    // Resize and relocate in one action: the entry freezes on the rect the window
    // occupies now, while the stage already holds the far-away destination.
    f.state()
        .animate_window_geometry(&window, Size::from((900, 700)), None);
    f.state()
        .map_window(window.clone(), Point::from((6000, 6000)), false);
    assert!(f.state().window_animations.start_held(eid), "frozen");

    let bbox = f
        .state()
        .window_bbox_with_popups(&window)
        .expect("the window is stage-mapped");
    assert!(
        !f.state().canvas_rect_drawable(bbox),
        "the live rect has left every viewport"
    );
    let culled = f.state().window_cull_rect(Some(eid), bbox);
    assert!(
        f.state().canvas_rect_drawable(culled),
        "but the picture on screen has not, so the frame must still draw it"
    );
}

/// A resize below the sub-threshold floor carries no request at all — nothing
/// worth freezing over (`MIN_ANIMATED_RESIZE` in `state::window_animation`).
#[test]
fn a_sub_threshold_resize_carries_no_request() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let _surface = map_window(&mut f, id, "a", (400, 300));
    let window = window_by_app_id(&mut f, "a").unwrap();
    reset_view(&mut f);
    f.state()
        .map_window(window.clone(), Point::from((600, 400)), false);
    let eid = element_id(&mut f, &window);
    tick_until_settled(&mut f);

    let committed = window.geometry().size;
    f.state().animate_window_geometry(
        &window,
        Size::from((committed.w + 10, committed.h + 3)),
        None,
    );
    assert!(
        !f.state().window_animations.start_held(eid),
        "a resize this small has nothing worth waiting for"
    );
    // No budget was opened, so the leg converges on its own — with a freeze armed
    // this spins until the deadline instead.
    tick_until_settled(&mut f);

    f.state()
        .animate_window_geometry(&window, Size::from((committed.w + 11, committed.h)), None);
    assert!(
        f.state().window_animations.start_held(eid),
        "one pixel more is a real resize, and freezes like one"
    );
}

/// The mirror: an exit armed while the *enter* is still frozen must not strip the
/// chrome off the windowed picture that freeze is holding. Seeded with a restore
/// size the client never had, so the exit restores to a size that isn't on screen
/// and the retarget genuinely carries a request.
#[test]
fn a_fullscreen_exit_during_the_enter_freeze_keeps_the_windowed_chrome() {
    let mut f = Fixture::new();
    let output = f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let surface = map_window(&mut f, id, "fs", (400, 300));
    let window = window_by_app_id(&mut f, "fs").unwrap();
    reset_view(&mut f);
    f.state()
        .map_window(window.clone(), Point::from((600, 400)), false);
    let eid = element_id(&mut f, &window);
    tick_until_settled(&mut f);

    // What a resize settle leaves behind once the client shrinks itself again:
    // the size the exit will restore to is not the one on screen.
    f.state()
        .stage
        .set_restore_size(&window, Size::from((520, 380)));

    f.client(id).window(&surface).set_fullscreen(None);
    f.double_roundtrip(id);
    let generation = f.state().window_animations.generation_of(eid);
    assert!(
        f.state().window_animations.start_held(eid),
        "the enter waits for the client to redraw fullscreen"
    );
    assert!(
        !f.state().chrome_fullscreen(&window),
        "the picture on screen is still the windowed one"
    );

    // Fullscreen off again, inside the same freeze.
    f.state().exit_fullscreen_on(&output);
    assert!(
        f.state().window_animations.generation_of(eid) > generation,
        "restoring a size the client does not have is a new request"
    );
    assert!(
        !f.state().chrome_fullscreen(&window),
        "the frame never became fullscreen, so it keeps its chrome"
    );

    super::adopt_last_configure(&mut f, id, &surface);
    tick_until_settled(&mut f);
}

/// A frozen window paints the identical picture every frame, so it must not
/// drive a full compose per tick for half a second. It still has to count as an
/// active animation, though: the deadline that ends the freeze can only fire from
/// a tick, and the tick that fires it does move the window.
#[test]
fn a_frozen_entry_asks_for_no_redraw_but_keeps_ticking() {
    let mut f = Fixture::new();
    let output = f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let _surface = map_window(&mut f, id, "a", (400, 300));
    let window = window_by_app_id(&mut f, "a").unwrap();
    reset_view(&mut f);
    f.state()
        .map_window(window.clone(), Point::from((600, 400)), false);
    let eid = element_id(&mut f, &window);
    tick_until_settled(&mut f);

    f.state().fit_window(&window);
    let base = Instant::now();
    f.state().tick_window_animations_at(TICK, base);
    assert!(
        f.state().window_animations.start_held(eid),
        "the fit froze the window"
    );

    f.state().redraws_needed.clear();
    f.state().tick_window_animations_at(TICK, base);
    assert!(
        f.state().redraws_needed.is_empty(),
        "a frozen tick composes nothing new"
    );
    assert!(
        f.state().output_has_active_animations(&output),
        "but the entry still keeps the loop awake, or its budget could never expire"
    );

    // The tick that lets the budget expire is a real frame: it starts the leg.
    let past = base + PAST_HOLD;
    f.state().tick_window_animations_at(TICK, past);
    assert!(
        !f.state().redraws_needed.is_empty(),
        "the tick that unfreezes the window marks its output"
    );
}

/// A request for the size the window already has resolves at the seed, so there is
/// nothing to wait for: no freeze, and the leg runs immediately.
#[test]
fn a_same_size_request_never_freezes() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let _surface = map_window(&mut f, id, "a", (400, 300));
    let window = window_by_app_id(&mut f, "a").unwrap();
    reset_view(&mut f);
    f.state()
        .map_window(window.clone(), Point::from((400, 300)), false);
    let eid = element_id(&mut f, &window);
    tick_until_settled(&mut f);

    let committed = window.geometry().size;
    f.state().animate_window_geometry(&window, committed, None);
    f.state()
        .map_window(window.clone(), Point::from((700, 300)), false);
    assert!(
        !f.state().window_animations.start_held(eid),
        "an already-satisfied request has nothing to wait for"
    );
    tick_until_settled(&mut f);
}

/// A brand new resize landing mid-anything re-freezes from wherever the window is
/// and bumps the capture generation, so content captured for the superseded
/// request can never be paired with the new leg.
#[test]
fn a_request_carrying_retarget_refreezes_and_bumps_the_generation() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let _surface = map_window(&mut f, id, "a", (400, 300));
    let window = window_by_app_id(&mut f, "a").unwrap();
    reset_view(&mut f);
    f.state()
        .map_window(window.clone(), Point::from((600, 400)), false);
    let eid = element_id(&mut f, &window);
    tick_until_settled(&mut f);

    f.state().fit_window(&window);
    let first = f.state().window_animations.generation_of(eid).unwrap();
    let base = Instant::now();
    for _ in 0..4 {
        f.state().tick_window_animations_at(TICK, base);
    }
    seed_resize_capture(&mut f, eid);

    // A second, genuinely different resize while the first is still frozen.
    // (Unfitting back to the size the client still has would be a same-size
    // request, resolved at the seed with nothing new to wait for.)
    f.state()
        .animate_window_geometry(&window, Size::from((900, 700)), None);
    let second = f.state().window_animations.generation_of(eid).unwrap();
    assert!(
        second > first,
        "the new request invalidates the old capture ({first} -> {second})"
    );
    assert!(
        f.state().window_animations.start_held(eid),
        "and it waits for the client's redraw of the new size"
    );
    let counters = f.state().debug_counters();
    assert_eq!(
        counters["resize_captures"], 0,
        "the superseded request's capture went with it"
    );
    assert_eq!(
        counters["resize_crossfades"], 0,
        "as would any overlay for the leg that no longer exists — that half needs \
         a renderer, so only the capture is pinned here"
    );
}

/// A freeze whose window scrolls off every viewport instant-completes, and the
/// content captured for a crossfade that will never play goes with it. The
/// client's eventual redraw then finds nothing to resolve.
#[test]
fn an_off_screen_freeze_drops_its_entry_and_its_capture() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let surface = map_window(&mut f, id, "a", (400, 300));
    let window = window_by_app_id(&mut f, "a").unwrap();
    reset_view(&mut f);
    f.state()
        .map_window(window.clone(), Point::from((600, 400)), false);
    let eid = element_id(&mut f, &window);
    tick_until_settled(&mut f);

    f.state().fit_window(&window);
    let base = Instant::now();
    f.state().tick_window_animations_at(TICK, base);
    assert!(
        f.state().window_animations.start_held(eid),
        "the fit froze the window"
    );
    seed_resize_capture(&mut f, eid);

    // Pan away: the frozen rect now intersects no viewport.
    f.state().set_camera(Point::from((100_000.0, 100_000.0)));
    f.state().update_output_from_camera();
    f.state().tick_window_animations_at(TICK, base);

    assert_eq!(
        f.state().window_animations.len(),
        0,
        "the frozen entry instant-completed off-screen"
    );
    let counters = f.state().debug_counters();
    assert_eq!(
        counters["resize_captures"], 0,
        "nothing stays stashed for a leg that will never run"
    );
    assert_eq!(counters["resize_crossfades"], 0, "overlay is backend-gated");

    // The redraw the freeze was waiting for lands late: a no-op, not a revival.
    let w = f.client(id).window(&surface);
    w.set_size(1896, 1056);
    w.attach_new_buffer();
    w.ack_last_and_commit();
    f.double_roundtrip(id);
    assert_eq!(
        f.state().window_animations.len(),
        0,
        "the late commit revived nothing"
    );
    let counters = f.state().debug_counters();
    assert_eq!(counters["resize_captures"], 0);
    assert_eq!(counters["resize_crossfades"], 0);
}

/// Suspending a window mid-freeze converts it into a stand-in that inherits its
/// `ElementId`, so both crossfade halves have to be dropped at the conversion:
/// the dead-id sweep can never fire for an id that is still very much alive, and
/// a surviving overlay would wear the dead client's pixels on the stand-in.
#[test]
fn conversion_mid_freeze_drops_the_crossfade_with_the_entry() {
    let tmp = TempDir::new();
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    // A stand-in only appears for an app the compositor can relaunch.
    std::fs::write(
        tmp.path().join("myapp.desktop"),
        "[Desktop Entry]\nType=Application\nName=myapp\nExec=myapp\n",
    )
    .unwrap();
    f.state().desktop_entry_cache = Some(DesktopEntryCache::new(vec![tmp.path().to_path_buf()]));
    let id = f.add_client();
    let surface = map_window(&mut f, id, "myapp", (400, 300));
    let window = window_by_app_id(&mut f, "myapp").unwrap();
    reset_view(&mut f);
    f.state()
        .map_window(window.clone(), Point::from((600, 400)), false);
    let eid = element_id(&mut f, &window);
    tick_until_settled(&mut f);
    let serial = smithay::utils::SERIAL_COUNTER.next_serial();
    f.state().raise_and_focus(&window, serial);

    f.state().fit_window(&window);
    f.state().tick_window_animations_at(TICK, Instant::now());
    assert!(
        f.state().window_animations.start_held(eid),
        "the fit froze the window"
    );
    seed_resize_capture(&mut f, eid);

    f.state().execute_action(&Action::SuspendWindow);
    f.client(id).window(&surface).destroy();
    f.roundtrip(id);
    f.dispatch();

    assert!(
        f.state()
            .stage
            .window_by_id(eid)
            .is_some_and(|w| w.suspended().is_some()),
        "the stand-in inherited the frozen window's id — no sweep will collect it"
    );
    assert_eq!(
        f.state().window_animations.len(),
        0,
        "the conversion dropped the frozen chase"
    );
    let counters = f.state().debug_counters();
    assert_eq!(
        counters["resize_captures"], 0,
        "and the content captured for its crossfade"
    );
    assert_eq!(counters["resize_crossfades"], 0, "overlay is backend-gated");

    // Tear the stand-in down for the baseline.
    let sid = f
        .state()
        .stage
        .windows()
        .find_map(|w| w.suspended().map(|s| s.id));
    if let Some(sid) = sid {
        f.state().dismiss_suspended(sid);
    }
}

/// A window that remaps mid-freeze (a hide-to-tray reshow) gets an open entry
/// written straight over its geometry entry — there is no remove site to hang
/// the cleanup on — so the crossfade halves have to go at the open itself.
/// Otherwise the old picture keeps fading over a window that is scaling in.
#[test]
fn an_open_entry_over_a_frozen_resize_drops_the_crossfade() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let _surface = map_window(&mut f, id, "a", (400, 300));
    let window = window_by_app_id(&mut f, "a").unwrap();
    reset_view(&mut f);
    f.state()
        .map_window(window.clone(), Point::from((600, 400)), false);
    let eid = element_id(&mut f, &window);
    tick_until_settled(&mut f);

    f.state().fit_window(&window);
    f.state().tick_window_animations_at(TICK, Instant::now());
    assert!(
        f.state().window_animations.start_held(eid),
        "the fit froze the window"
    );
    seed_resize_capture(&mut f, eid);

    f.state().start_window_open_animation(&window);
    assert!(
        !f.state().window_animations.start_held(eid),
        "the open entry replaced the frozen chase"
    );
    assert_eq!(
        f.state().debug_counters()["resize_captures"],
        0,
        "and its captured content went with it"
    );
    tick_until_settled(&mut f);
}

/// The old-content bake is rasterized for the size it will be drawn at: one baked
/// texel per physical pixel, at every output scale and camera zoom. The render
/// side draws the bake across the entry's visual rect through the window's own
/// transform, so the drawn extent is `visual · zoom · output_scale` — flooring
/// the `output_scale · zoom` half at 1.0 (the close bake's rule) multiplies with
/// a fullscreen exit's `1/zoom` stretch and bakes a texture several times that:
/// 4x the pixels at zoom 0.5, 25x at 0.2, past `GL_MAX_TEXTURE_SIZE` below that,
/// where the allocation fails and the crossfade is silently skipped.
#[test]
fn a_resize_bake_carries_one_texel_per_drawn_pixel() {
    for scale in [1.0, 1.5, 2.0] {
        for zoom in [1.0, 0.75, 0.5, 0.2] {
            let mut f = Fixture::new();
            let output = f.add_output(1, (1920, 1080));
            output.change_current_state(
                None,
                None,
                Some(smithay::output::Scale::Fractional(scale)),
                None,
            );
            let id = f.add_client();
            let _surface = map_window(&mut f, id, "a", (800, 600));
            let window = window_by_app_id(&mut f, "a").unwrap();
            reset_view(&mut f);
            f.state().with_output_state(|os| os.zoom = zoom);
            f.state().update_output_from_camera();
            f.state()
                .map_window(window.clone(), Point::from((100, 100)), false);
            let eid = element_id(&mut f, &window);
            tick_until_settled(&mut f);

            // A fullscreen exit's shape: the captured picture is the fullscreen
            // buffer (one viewport), frozen on a canvas rect of `viewport / zoom`
            // while it restores to the windowed size.
            let captured = crate::state::output_logical_size(&output);
            let seed = Rectangle::new(
                Point::from((100.0, 100.0)),
                Size::from((captured.w as f64 / zoom, captured.h as f64 / zoom)),
            );
            f.state().begin_geometry_animation_seeded(
                &window,
                seed,
                crate::state::window_animation::AnimSpace::Canvas,
                Some(Size::from((800, 600))),
                crate::state::window_animation::GeometryRole::FullscreenExit {
                    output: output.name(),
                },
                crate::state::window_animation::ContentPolicy::Cap,
                None,
            );

            let visual = f
                .state()
                .window_animations
                .geometry_visual_rect(eid)
                .expect("the exit seeded a frozen entry");
            let texels = captured.w as f64 * f.state().resize_bake_scale(&window, eid, captured);
            let drawn = visual.size.w * zoom * scale;
            assert!(
                (texels / drawn - 1.0).abs() < 1e-6,
                "scale {scale}, zoom {zoom}: baked {texels:.0} texels for {drawn:.0} \
                 drawn px"
            );
        }
    }
}

/// The close bake keeps its floor: the resize bake's unfloored scale is a
/// sibling, not a replacement. A close fades in canvas space, so a snapshot taken
/// while zoomed out still rasterizes at full logical resolution.
#[test]
fn a_close_bake_never_rasterizes_below_logical_resolution() {
    let mut f = Fixture::new();
    let output = f.add_output(1, (1920, 1080));
    reset_view(&mut f);
    let rect = Rectangle::new(Point::from((100, 100)), Size::from((400, 300)));

    f.state().with_output_state(|os| os.zoom = 0.5);
    f.state().update_output_from_camera();
    assert_eq!(
        f.state().flatten_scale_for_canvas_rect(rect),
        1.0,
        "zoomed out, the floor holds the bake at logical resolution"
    );

    output.change_current_state(
        None,
        None,
        Some(smithay::output::Scale::Fractional(2.0)),
        None,
    );
    f.state().with_output_state(|os| os.zoom = 1.0);
    f.state().update_output_from_camera();
    assert_eq!(
        f.state().flatten_scale_for_canvas_rect(rect),
        2.0,
        "above the floor the rect's render scale is used as-is"
    );

    let off_screen = Rectangle::new(Point::from((100_000, 100_000)), Size::from((400, 300)));
    assert_eq!(
        f.state().flatten_scale_for_canvas_rect(off_screen),
        1.0,
        "a rect no output shows falls back to the same floor"
    );
}

/// Adoption holds a slot rather than requesting a resize, so it is never frozen —
/// its content is meant to stretch to fill immediately.
#[test]
fn an_adopted_slot_is_never_frozen() {
    let tmp = TempDir::new();
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    let (_sid, cid, surface) = arrange_pending_relaunch(&mut f, &tmp);
    let w = f.client(cid).window(&surface);
    w.set_size(300, 200);
    w.attach_new_buffer();
    w.ack_last_and_commit();
    f.double_roundtrip(cid);

    let adopted = window_by_app_id(&mut f, "myapp").expect("adopted the slot");
    let eid = element_id(&mut f, &adopted);
    assert!(
        !f.state().window_animations.start_held(eid),
        "a Stretch entry never start-holds"
    );
}

/// Fullscreen a window with the camera parked far from the origin, so
/// `HomeToggle` reads "not at home" and takes its go-home branch.
fn fullscreen_away_from_home(f: &mut Fixture) -> (ClientId, ClientSurface, Window) {
    let output = f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let surface = map_window(f, id, "fs", (800, 600));
    let window = window_by_app_id(f, "fs").unwrap();
    reset_view(f);
    f.state().set_camera(Point::from((5000.0, 5000.0)));
    f.state().update_output_from_camera();

    f.state().enter_fullscreen(&window, Some(output));
    f.double_roundtrip(id);
    super::adopt_last_configure(f, id, &surface);
    tick_until_settled(f);

    let serial = smithay::utils::SERIAL_COUNTER.next_serial();
    f.state().raise_and_focus(&window, serial);
    (id, surface, window)
}

/// Going home from fullscreen snaps the camera in one frame, so the window snaps
/// with it: no geometry entry outlives the action, nothing stays frozen, and the
/// windowed chrome is there the moment it returns.
#[test]
fn home_toggle_leaves_fullscreen_instantly() {
    let mut f = Fixture::new();
    let (_id, _surface, window) = fullscreen_away_from_home(&mut f);
    let eid = element_id(&mut f, &window);
    seed_resize_capture(&mut f, eid);

    f.state().execute_action(&Action::HomeToggle);

    assert!(
        !f.state().stage.is_fullscreen(&window),
        "the action left fullscreen"
    );
    assert_eq!(
        f.state().window_animations.len(),
        0,
        "leaving no geometry entry to play out over the snapped camera"
    );
    assert_eq!(
        chrome_alpha(&mut f, &window),
        1.0,
        "so the windowed chrome is already back, with no frozen picture holding it off"
    );
    assert_eq!(
        f.state().debug_counters()["resize_captures"],
        0,
        "and no content stashed for the crossfade that never runs"
    );
    assert!(
        f.state().camera_target().is_none() && f.state().zoom_target().is_none(),
        "the camera half stays the instant snap it always was"
    );
}

/// The return trip is the same deal from the other side: the second HomeToggle
/// re-enters fullscreen with the camera set directly, so the window arrives at
/// full size rather than growing into it.
#[test]
fn home_toggle_returns_to_fullscreen_instantly() {
    let mut f = Fixture::new();
    let (id, surface, window) = fullscreen_away_from_home(&mut f);

    f.state().execute_action(&Action::HomeToggle);
    f.double_roundtrip(id);
    super::adopt_last_configure(&mut f, id, &surface);
    assert_eq!(
        f.state().window_animations.len(),
        0,
        "the trip out left nothing running"
    );

    f.state().execute_action(&Action::HomeToggle);

    assert!(
        f.state().stage.is_fullscreen(&window),
        "the saved fullscreen came back"
    );
    assert_eq!(
        f.state().window_animations.len(),
        0,
        "without a growth leg into the locked viewport"
    );
    assert!(
        f.state().chrome_fullscreen(&window),
        "and with the fullscreen look already on screen"
    );
}

/// A touch tier-crossing exits fullscreen before the action is even dispatched,
/// so the leg it arms is past the guard in `execute_action` by the time
/// HomeToggle runs. The snap has to take that one down too.
#[test]
fn home_toggle_after_a_pre_exited_fullscreen_is_instant() {
    let mut f = Fixture::new();
    let (_id, _surface, window) = fullscreen_away_from_home(&mut f);
    let output = f.state().active_output().unwrap();

    // What the touch grab does ahead of dispatching the threshold action.
    f.state().pre_exited_fullscreen = Some(window.clone());
    f.state().exit_fullscreen_on(&output);
    assert_eq!(
        f.state().window_animations.len(),
        1,
        "the pre-exit armed a shrink leg"
    );

    f.state().execute_action(&Action::HomeToggle);

    assert_eq!(
        f.state().window_animations.len(),
        0,
        "which the snap took down with it"
    );
    assert_eq!(
        chrome_alpha(&mut f, &window),
        1.0,
        "leaving the windowed picture on screen, not a frozen fullscreen one"
    );
}

/// The instant paths are the ones that snap: the plain fullscreen keybinding
/// still animates in both directions.
#[test]
fn toggle_fullscreen_still_animates() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let surface = map_window(&mut f, id, "fs", (800, 600));
    let window = window_by_app_id(&mut f, "fs").unwrap();
    reset_view(&mut f);
    tick_until_settled(&mut f);
    let serial = smithay::utils::SERIAL_COUNTER.next_serial();
    f.state().raise_and_focus(&window, serial);

    f.state().execute_action(&Action::ToggleFullscreen);
    assert_eq!(
        f.state().window_animations.len(),
        1,
        "entering fullscreen animates"
    );
    f.double_roundtrip(id);
    super::adopt_last_configure(&mut f, id, &surface);
    tick_until_settled(&mut f);

    f.state().execute_action(&Action::ToggleFullscreen);
    assert_eq!(
        f.state().window_animations.len(),
        1,
        "and so does leaving it"
    );
    f.double_roundtrip(id);
    super::adopt_last_configure(&mut f, id, &surface);
    tick_until_settled(&mut f);
}

/// Sending a fullscreen window to the next monitor is a teleport, not a second
/// fullscreen entry: it is already at full size, so nothing animates.
#[test]
fn send_to_output_moves_a_fullscreen_window_instantly() {
    let mut f = Fixture::new();
    let out1 = f.add_output(1, (1920, 1080));
    let out2 = f.add_output(2, (1280, 720));
    let id = f.add_client();
    let surface = map_window(&mut f, id, "fs", (400, 300));
    let window = window_by_app_id(&mut f, "fs").unwrap();
    let eid = element_id(&mut f, &window);

    f.state().enter_fullscreen(&window, Some(out1));
    f.double_roundtrip(id);
    super::adopt_last_configure(&mut f, id, &surface);
    tick_until_settled(&mut f);
    seed_resize_capture(&mut f, eid);

    f.state()
        .execute_action(&Action::SendToOutput(Direction::Right));

    assert_eq!(
        f.state().stage.fullscreen_output_of(&window),
        Some(out2.name().as_str()),
        "the window moved monitor"
    );
    assert_eq!(
        f.state().window_animations.len(),
        0,
        "with no geometry entry left by either half of the handover"
    );
    assert_eq!(
        f.state().debug_counters()["resize_captures"],
        0,
        "and no content stashed for a leg that never runs"
    );
    assert!(
        f.state().chrome_fullscreen(&window),
        "and no windowed chrome flashing over the move"
    );
}

/// The plain canvas case is a stage reposition and always was instant — pin it so
/// a future move path can't quietly grow a leg.
#[test]
fn send_to_output_moves_a_canvas_window_instantly() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let out2 = f.add_output(2, (1280, 720));
    // Both outputs default to a camera on the canvas origin, so their viewports
    // overlap; pan out2 away so output_for_window can tell where the window landed.
    crate::state::output_state(&out2).camera = Point::from((5000.0, 5000.0));
    let id = f.add_client();
    map_window(&mut f, id, "a", (400, 300));
    let window = window_by_app_id(&mut f, "a").unwrap();
    reset_view(&mut f);
    tick_until_settled(&mut f);
    let serial = smithay::utils::SERIAL_COUNTER.next_serial();
    f.state().raise_and_focus(&window, serial);

    f.state()
        .execute_action(&Action::SendToOutput(Direction::Right));

    assert_eq!(
        f.state().output_for_window(&window).map(|o| o.name()),
        Some(out2.name()),
        "the window moved monitor"
    );
    assert_eq!(
        f.state().window_animations.len(),
        0,
        "without arming a move leg"
    );
}

/// Pin/unpin flips the window between canvas and screen space — at zoom != 1 its
/// on-screen size changes outright, so both directions animate the flip instead
/// of jumping. The window's own content never crossfades: there is no resize to
/// wait on, so any stashed content goes down with the superseded entry.
#[test]
fn pin_toggle_animates_the_space_flip_in_both_directions() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    map_window(&mut f, id, "a", (400, 300));
    let window = window_by_app_id(&mut f, "a").unwrap();
    reset_view(&mut f);
    f.state().with_output_state(|os| os.zoom = 0.5);
    f.state().update_output_from_camera();
    tick_until_settled(&mut f);
    let eid = element_id(&mut f, &window);
    let serial = smithay::utils::SERIAL_COUNTER.next_serial();
    f.state().raise_and_focus(&window, serial);

    seed_resize_capture(&mut f, eid);
    f.state().execute_action(&Action::TogglePinToScreen);
    assert!(f.state().is_pinned(&window), "the window pinned");
    assert_eq!(
        f.state().window_animations.len(),
        1,
        "pinning armed an entry to play the space flip over"
    );
    assert_eq!(
        f.state().debug_counters()["resize_captures"],
        0,
        "and dropped the content stashed for a resize that isn't happening"
    );
    tick_until_settled(&mut f);

    seed_resize_capture(&mut f, eid);
    f.state().execute_action(&Action::TogglePinToScreen);
    assert!(!f.state().is_pinned(&window), "the window unpinned");
    assert_eq!(
        f.state().window_animations.len(),
        1,
        "and unpinning armed one too"
    );
    assert_eq!(
        f.state().debug_counters()["resize_captures"],
        0,
        "with the stash dropped again"
    );
}

/// The fit's own destination, computed the same way `compute_fit_geometry` does:
/// the usable area's center subtracted from the window's pre-fit visual center.
/// Read before `fit_window` runs, since that is the state its internal
/// computation sees too.
fn fit_target_camera(f: &mut Fixture, window: &Window) -> Point<f64, Logical> {
    let usable = f.state().get_usable_area();
    let usable_center: Point<f64, Logical> = Point::from((
        usable.loc.x as f64 + usable.size.w as f64 / 2.0,
        usable.loc.y as f64 + usable.size.h as f64 / 2.0,
    ));
    let visual_center = f.state().window_visual_center(window).unwrap();
    Point::from((
        visual_center.x - usable_center.x,
        visual_center.y - usable_center.y,
    ))
}

/// The camera pan for an off-center fit is parked behind the window's resize
/// freeze, not armed at the action — otherwise the pan and the resize read as
/// two separate motions instead of one.
#[test]
fn a_fit_on_an_off_center_window_parks_its_pan_until_the_ack() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let surface = map_window(&mut f, id, "a", (400, 300));
    let window = window_by_app_id(&mut f, "a").unwrap();
    reset_view(&mut f);
    f.state()
        .map_window(window.clone(), Point::from((600, 400)), false);
    let eid = element_id(&mut f, &window);
    tick_until_settled(&mut f);

    let want_camera = fit_target_camera(&mut f, &window);
    assert!(
        dist(want_camera, Point::from((0.0, 0.0))) > 50.0,
        "the fixture must actually be off-center, or a zero-length pan would \
         pass this test whether or not it ever ran"
    );

    f.state().fit_window(&window);
    assert!(
        f.state().window_animations.start_held(eid),
        "the resize is well above the sub-threshold floor, so it freezes"
    );
    assert!(
        f.state().camera_target().is_none(),
        "the pan is parked behind the freeze, not armed at the action"
    );

    f.double_roundtrip(id);
    super::adopt_last_configure(&mut f, id, &surface);
    f.double_roundtrip(id);
    assert!(
        f.state().camera_target().is_none(),
        "the ack alone does not arm it — a window-animation tick has to see \
         the release"
    );

    f.state().tick_window_animations(TICK);
    let camera_target = f.state().camera_target();
    assert!(
        camera_target.is_some_and(|c| dist(c, want_camera) < 1e-6),
        "the released pan lands on the fit's own destination, got \
         {camera_target:?} want {want_camera:?}"
    );
}

/// The far end a client never acks: the parked pan is still waiting behind the
/// freeze right up to the budget's edge, and the degrade that finally moves the
/// leg (with stale content) releases the pan along with it — the same tick, the
/// same predicate as the commit path.
#[test]
fn a_fit_that_is_never_acked_parks_its_pan_until_the_degrade() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let _surface = map_window(&mut f, id, "a", (400, 300));
    let window = window_by_app_id(&mut f, "a").unwrap();
    reset_view(&mut f);
    f.state()
        .map_window(window.clone(), Point::from((600, 400)), false);
    let eid = element_id(&mut f, &window);
    tick_until_settled(&mut f);

    let want_camera = fit_target_camera(&mut f, &window);

    f.state().fit_window(&window);
    assert!(
        f.state().camera_target().is_none(),
        "parked behind the freeze"
    );

    let base = Instant::now();
    for _ in 0..30 {
        f.state().tick_window_animations_at(TICK, base);
    }
    assert!(
        f.state().window_animations.start_held(eid),
        "still frozen — nothing has acked"
    );
    assert!(
        f.state().camera_target().is_none(),
        "still parked while the freeze holds"
    );

    let past = base + PAST_HOLD;
    f.state().tick_window_animations_at(TICK, past);
    assert!(
        !f.state().window_animations.start_held(eid),
        "the budget expiring degrades the leg"
    );
    let camera_target = f.state().camera_target();
    assert!(
        camera_target.is_some_and(|c| dist(c, want_camera) < 1e-6),
        "the degrade releases the parked pan too, got {camera_target:?} want \
         {want_camera:?}"
    );
}

/// Two fits inside one freeze: the second one owns the camera. Both park a pan
/// on their own window's entry, both stamp the same untouched viewport, and
/// nothing about the payloads says which came last — so without a sweep the
/// camera lands wherever the first client to redraw happens to send it, which is
/// the window the user fitted *before* the one they just fitted. Reachable with
/// no user timing at all: a client that opens two windows both requesting
/// maximize produces exactly this.
#[test]
fn a_second_fit_supersedes_the_first_fits_parked_pan() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let first = map_window(&mut f, id, "a", (400, 300));
    let second = map_window(&mut f, id, "b", (400, 300));
    let w1 = window_by_app_id(&mut f, "a").unwrap();
    let w2 = window_by_app_id(&mut f, "b").unwrap();
    reset_view(&mut f);
    f.state()
        .map_window(w1.clone(), Point::from((100, 100)), false);
    f.state()
        .map_window(w2.clone(), Point::from((1300, 700)), false);
    tick_until_settled(&mut f);

    let superseded = fit_target_camera(&mut f, &w1);
    f.state().fit_window(&w1);
    let want_camera = fit_target_camera(&mut f, &w2);
    assert!(
        dist(superseded, want_camera) > 50.0,
        "the two fits must aim somewhere different, or the camera assertion \
         below passes whichever pan wins"
    );
    f.state().fit_window(&w2);
    let (eid1, eid2) = (element_id(&mut f, &w1), element_id(&mut f, &w2));
    assert!(
        f.state().window_animations.start_held(eid1)
            && f.state().window_animations.start_held(eid2),
        "precondition: both fits froze, so both had a pan to park"
    );

    // The first window redraws first, so its freeze is the first to release.
    f.double_roundtrip(id);
    super::adopt_last_configure(&mut f, id, &first);
    f.state().tick_window_animations(TICK);
    assert!(
        f.state().camera_target().is_none(),
        "the first fit's pan belongs to an action the second one superseded"
    );

    super::adopt_last_configure(&mut f, id, &second);
    f.state().tick_window_animations(TICK);
    let camera_target = f.state().camera_target();
    assert!(
        camera_target.is_some_and(|c| dist(c, want_camera) < 1e-6),
        "the camera lands on the window that was fitted last, got \
         {camera_target:?} want {want_camera:?}"
    );
}

/// A resize too small to carry a request has nothing to freeze over, so there
/// is nothing for the pan to wait on either — it arms at the action.
#[test]
fn a_sub_threshold_fit_arms_its_pan_immediately() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let surface = map_window(&mut f, id, "a", (400, 300));
    let window = window_by_app_id(&mut f, "a").unwrap();
    reset_view(&mut f);
    let eid = element_id(&mut f, &window);
    tick_until_settled(&mut f);

    // Fit once for real, and let the client catch up, so the window is already
    // sitting at the fit's own target size.
    f.state().fit_window(&window);
    f.double_roundtrip(id);
    super::adopt_last_configure(&mut f, id, &surface);
    f.double_roundtrip(id);
    tick_until_settled(&mut f);

    // Fitting an already-fit window asks for the same size it already has: the
    // resize carries no request at all, so nothing freezes.
    f.state().fit_window(&window);
    assert!(
        !f.state().window_animations.start_held(eid),
        "a sub-threshold resize has nothing to freeze over"
    );
    assert!(
        f.state().camera_target().is_some(),
        "with no freeze to wait on, the pan arms immediately instead of parking"
    );
}

/// A camera move that lands during the freeze — a pan, momentum, a navigation
/// action — takes ownership of the viewport. The fit's own pan must not yank it
/// back once the freeze finally releases; it is dropped instead.
#[test]
fn a_camera_move_during_a_fit_freeze_drops_the_parked_pan() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let surface = map_window(&mut f, id, "a", (400, 300));
    let window = window_by_app_id(&mut f, "a").unwrap();
    reset_view(&mut f);
    f.state()
        .map_window(window.clone(), Point::from((600, 400)), false);
    tick_until_settled(&mut f);

    f.state().fit_window(&window);
    assert!(
        f.state().camera_target().is_none(),
        "parked behind the freeze"
    );

    // The user pans mid-freeze: a deliberate move that takes the viewport.
    let moved_to = Point::from((123.0, 45.0));
    f.state().set_camera(moved_to);

    f.double_roundtrip(id);
    super::adopt_last_configure(&mut f, id, &surface);
    f.double_roundtrip(id);
    f.state().tick_window_animations(TICK);

    assert!(
        f.state().camera_target().is_none(),
        "the parked pan is dropped, not applied over the user's move"
    );
    assert_eq!(
        f.state().camera(),
        moved_to,
        "and the camera is left exactly where the user put it"
    );
}

/// A request-carrying retarget mid-freeze — fullscreen entering, then leaving,
/// both before the fit's own freeze ever gets acked — supersedes the fit's
/// request each time, and the second's own action owns the transition from
/// there: the fit's parked pan does not survive to whatever release finally
/// comes.
#[test]
fn fullscreen_mid_fit_freeze_supersedes_the_parked_pan() {
    let mut f = Fixture::new();
    let output = f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let _surface = map_window(&mut f, id, "a", (400, 300));
    let window = window_by_app_id(&mut f, "a").unwrap();
    reset_view(&mut f);
    f.state()
        .map_window(window.clone(), Point::from((600, 400)), false);
    let eid = element_id(&mut f, &window);
    tick_until_settled(&mut f);

    f.state().fit_window(&window);
    assert!(
        f.state().camera_target().is_none(),
        "parked behind the fit's freeze"
    );

    f.state().enter_fullscreen(&window, Some(output.clone()));
    assert!(
        f.state().window_animations.start_held(eid),
        "the entry is still frozen, now on the fullscreen request"
    );

    f.state().exit_fullscreen_on(&output);
    assert!(
        !f.state().is_output_fullscreen(&output),
        "back out before ever acking either configure"
    );

    // Neither retarget was ever acked, so only the degrade ends the freeze —
    // the same predicate that fires the pending pan on a real commit.
    let base = Instant::now();
    for _ in 0..30 {
        f.state().tick_window_animations_at(TICK, base);
    }
    assert!(
        f.state().window_animations.start_held(eid),
        "still frozen — nothing has acked"
    );
    let past = base + PAST_HOLD;
    f.state().tick_window_animations_at(TICK, past);
    assert!(
        !f.state().window_animations.start_held(eid),
        "the budget expiring degrades the leg"
    );

    assert!(
        f.state().camera_target().is_none(),
        "the fit's pan never lands once fullscreen has superseded its freeze"
    );
}

/// Cancelling a window's animation outright (`TogglePinToScreen` is one of
/// several actions that do) takes the geometry entry down with it — and any pan
/// parked on that entry goes too. A frozen entry cannot complete on its own, so
/// there is no later moment for the pan to land.
#[test]
fn cancelling_a_frozen_fit_drops_its_parked_pan() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let surface = map_window(&mut f, id, "a", (400, 300));
    let window = window_by_app_id(&mut f, "a").unwrap();
    reset_view(&mut f);
    f.state()
        .map_window(window.clone(), Point::from((600, 400)), false);
    tick_until_settled(&mut f);

    f.state().fit_window(&window);
    assert!(
        f.state().camera_target().is_none(),
        "parked behind the freeze"
    );

    f.state().execute_action(&Action::TogglePinToScreen);
    assert!(f.state().is_pinned(&window), "the toggle pinned the window");
    assert!(
        f.state().camera_target().is_none(),
        "cancelling took the geometry entry — and the pan parked on it — down \
         with it, so nothing was handed back"
    );

    // The client still eventually redraws at the size the (now-cancelled) fit
    // requested; that real commit must not resurrect a pan that was dropped.
    f.double_roundtrip(id);
    super::adopt_last_configure(&mut f, id, &surface);
    f.double_roundtrip(id);
    tick_until_settled(&mut f);
    assert!(
        f.state().camera_target().is_none(),
        "no late pan lands for a freeze that was cancelled, not released"
    );
}

/// A drag takes over the geometry of what it grabs, so the entry still chasing
/// that element toward an earlier destination has to go — otherwise the window
/// sits motionless under the cursor and rubber-bands over a full leg when the
/// chase releases. The end happens on the first motion, not at grab install:
/// the SSD title bar installs a move grab on every left press, including plain
/// focus clicks.
#[test]
fn a_resize_drag_ends_the_entry_it_fights() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    map_window(&mut f, id, "a", (400, 300));
    let window = window_by_app_id(&mut f, "a").unwrap();
    reset_view(&mut f);
    f.state()
        .map_window(window.clone(), Point::from((400, 300)), false);
    let eid = element_id(&mut f, &window);
    tick_until_settled(&mut f);
    let serial = smithay::utils::SERIAL_COUNTER.next_serial();
    f.state().raise_and_focus(&window, serial);

    // A nudge leaves the window travelling toward its new position.
    f.state()
        .execute_action(&Action::NudgeWindow(Direction::Right));
    assert_eq!(
        f.state().window_animations.len(),
        1,
        "precondition: the nudge armed a chase"
    );

    // The user grabs the right edge before it lands.
    let pos = f.state().stage.position_of(&window).unwrap();
    let grab_at = Point::from((pos.x as f64 + 390.0, pos.y as f64 + 150.0));
    assert!(f.state().try_start_gesture_resize(grab_at, false));
    assert_eq!(
        f.state().window_animations.len(),
        1,
        "installing the grab alone leaves the chase alone"
    );

    motion(&mut f, grab_at + Point::from((40.0, 0.0)));
    assert_eq!(
        f.state().window_animations.len(),
        0,
        "the first resizing motion ended the chase it was fighting"
    );
    assert!(
        f.state()
            .window_animations
            .geometry_visual_rect(eid)
            .is_none(),
        "nothing overrides the live rect any more, so the window tracks the drag"
    );
    end_grab(&mut f);
}

/// The stand-in half of the same rule, through the other grab type.
#[test]
fn a_stand_in_drag_ends_the_entry_it_fights() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    reset_view(&mut f);
    // The stand-in below has no client to close it, so it is still on the
    // stage at teardown — end off-baseline.
    f.skip_baseline_check();
    let sid = f.state().insert_suspended_for_test(
        1,
        Point::from((200, 200)),
        Size::from((300, 200)),
        "n",
        "N",
    );
    let element = standin_element(&mut f, sid);

    // Something displaced it; the slide is still running.
    f.state()
        .map_window(element.clone(), Point::from((900, 200)), false);
    f.state()
        .animate_element_move_from(&element, Point::from((200, 200)), None);
    assert_eq!(
        f.state().window_animations.len(),
        1,
        "precondition: the slide armed an entry"
    );

    let grab_at = Point::from((1000.0, 300.0));
    assert!(f.state().try_start_gesture_move(grab_at, false));
    assert_eq!(
        f.state().window_animations.len(),
        1,
        "installing the grab alone leaves the slide alone"
    );

    motion(&mut f, grab_at + Point::from((60.0, 20.0)));
    assert_eq!(
        f.state().window_animations.len(),
        0,
        "the first moving motion ended the slide it was fighting"
    );
    assert_eq!(
        f.state().stage.position_of(&element),
        Some(Point::from((960, 220))),
        "and the stand-in tracks the finger from its stage position"
    );
    end_grab(&mut f);
}

/// Ending an entry is not cancelling one: a fit parks its whole camera move on
/// the entry it freezes, and only that entry can ever hand it back. A drag takes
/// the *window* away from the fit, not the viewport — so the pan still lands,
/// unlike the `cancel_window_animation` above, which drops it deliberately.
#[test]
fn a_drag_interrupting_a_fit_still_lands_its_parked_pan() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    map_window(&mut f, id, "a", (400, 300));
    let window = window_by_app_id(&mut f, "a").unwrap();
    reset_view(&mut f);
    f.state()
        .map_window(window.clone(), Point::from((600, 400)), false);
    tick_until_settled(&mut f);

    f.state().fit_window(&window);
    assert!(
        f.state().camera_target().is_none(),
        "precondition: the fit parked its pan behind the freeze"
    );

    // The user drags the window inside the freeze.
    let pos = f.state().stage.position_of(&window).unwrap();
    let grab_at = Point::from((pos.x as f64 + 100.0, pos.y as f64 + 100.0));
    assert!(f.state().try_start_gesture_move(grab_at, false));
    motion(&mut f, grab_at + Point::from((50.0, 20.0)));

    assert_eq!(
        f.state().window_animations.len(),
        0,
        "the drag ended the fit's entry"
    );
    assert!(
        f.state().camera_target().is_some(),
        "the pan parked on that entry still lands — the drag took the window \
         away from the fit, not the view"
    );
    end_grab(&mut f);
}

/// The same rule on the other release path. A press with no motion leaves the
/// entry standing, so the client's own ack is what frees the parked pan — and
/// the user holding the fitted window is the one who asked for the fit, so it
/// still lands.
#[test]
fn a_fit_lands_its_parked_pan_on_a_commit_under_a_grab_on_its_own_window() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let surface = map_window(&mut f, id, "a", (400, 300));
    let window = window_by_app_id(&mut f, "a").unwrap();
    reset_view(&mut f);
    f.state()
        .map_window(window.clone(), Point::from((600, 400)), false);
    let eid = element_id(&mut f, &window);
    tick_until_settled(&mut f);

    let want_camera = fit_target_camera(&mut f, &window);
    f.state().fit_window(&window);
    assert!(
        f.state().camera_target().is_none(),
        "precondition: the fit parked its pan behind the freeze"
    );

    let pos = f.state().stage.position_of(&window).unwrap();
    let grab_at = Point::from((pos.x as f64 + 100.0, pos.y as f64 + 100.0));
    assert!(f.state().try_start_gesture_move(grab_at, false));
    assert!(
        f.state().window_animations.start_held(eid),
        "precondition: a press with no motion leaves the freeze standing"
    );

    f.double_roundtrip(id);
    super::adopt_last_configure(&mut f, id, &surface);
    f.double_roundtrip(id);
    f.state().tick_window_animations(TICK);

    let camera_target = f.state().camera_target();
    assert!(
        camera_target.is_some_and(|c| dist(c, want_camera) < 1e-6),
        "the drag holding the fitted window inherits its pan, got \
         {camera_target:?} want {want_camera:?}"
    );
    end_grab(&mut f);
}

/// A grab on some *other* window is not the fit's heir. Landing the pan under it
/// would slide the canvas out from under a drag that never asked for it — and it
/// would land more readily under a grab than with none at all, since the grab
/// install is what cleared the camera targets `apply_pending_view` treats as a
/// later action's claim. The pan is only held back, though: it lands once the
/// unrelated grab lets go.
#[test]
fn a_grab_on_another_window_holds_back_a_fits_parked_pan() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let surface = map_window(&mut f, id, "a", (400, 300));
    map_window(&mut f, id, "b", (400, 300));
    let fitted = window_by_app_id(&mut f, "a").unwrap();
    let other = window_by_app_id(&mut f, "b").unwrap();
    reset_view(&mut f);
    f.state()
        .map_window(fitted.clone(), Point::from((600, 400)), false);
    f.state()
        .map_window(other.clone(), Point::from((1300, 700)), false);
    tick_until_settled(&mut f);

    let want_camera = fit_target_camera(&mut f, &fitted);
    f.state().fit_window(&fitted);
    assert!(
        f.state().camera_target().is_none(),
        "precondition: the fit parked its pan behind the freeze"
    );

    assert!(
        f.state()
            .try_start_gesture_move(Point::from((1400.0, 800.0)), false)
    );

    f.double_roundtrip(id);
    super::adopt_last_configure(&mut f, id, &surface);
    f.double_roundtrip(id);

    // Past the whole geometry leg, not just the tick that releases the freeze:
    // convergence takes a fraction of a second and drops the entry the pan was
    // parked on, so anything merely *skipped* here is gone for good.
    tick_until_settled(&mut f);
    assert!(
        f.state().camera_target().is_none(),
        "the pan is held back for as long as an unrelated window is held"
    );

    end_grab(&mut f);

    let camera_target = f.state().camera_target();
    assert!(
        camera_target.is_some_and(|c| dist(c, want_camera) < 1e-6),
        "the pan was held back, not consumed, got {camera_target:?} want \
         {want_camera:?}"
    );
}

/// The gate has to answer "is a grab live", not "is an interactive move armed":
/// a client resize installs a grab measuring against the same frozen canvas
/// anchor and pushes nothing to `interactive_move`, since the surface's own
/// `ResizeState` is what witnesses it.
#[test]
fn a_client_resize_grab_holds_back_a_fits_parked_pan() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let surface = map_window(&mut f, id, "a", (400, 300));
    map_window(&mut f, id, "b", (400, 300));
    let fitted = window_by_app_id(&mut f, "a").unwrap();
    let other = window_by_app_id(&mut f, "b").unwrap();
    reset_view(&mut f);
    f.state()
        .map_window(fitted.clone(), Point::from((600, 400)), false);
    f.state()
        .map_window(other.clone(), Point::from((1300, 700)), false);
    tick_until_settled(&mut f);

    let want_camera = fit_target_camera(&mut f, &fitted);
    f.state().fit_window(&fitted);
    assert!(
        f.state().camera_target().is_none(),
        "precondition: the fit parked its pan behind the freeze"
    );

    let pointer = f.state().seat.get_pointer().unwrap();
    let serial = smithay::utils::SERIAL_COUNTER.next_serial();
    assert!(
        f.state().start_compositor_resize_with_edge(
            &pointer,
            &other,
            Point::from((1690.0, 850.0)),
            BTN_LEFT,
            serial,
            Some(xdg_toplevel::ResizeEdge::Right),
            false,
        ),
        "precondition: the client resize grab installed"
    );
    assert!(
        f.state().interactive_move.is_empty(),
        "precondition: and left no interactive-move entry to be found by"
    );

    f.double_roundtrip(id);
    super::adopt_last_configure(&mut f, id, &surface);
    f.double_roundtrip(id);

    tick_until_settled(&mut f);
    assert!(
        f.state().camera_target().is_none(),
        "the pan is held back for as long as the resize is held"
    );

    end_grab(&mut f);

    let camera_target = f.state().camera_target();
    assert!(
        camera_target.is_some_and(|c| dist(c, want_camera) < 1e-6),
        "and lands when the resize lets go, got {camera_target:?} want \
         {want_camera:?}"
    );
}

/// A pan staged for another output cannot reach the grab in the first place —
/// only the active output's camera warps the pointer. Holding it back would be
/// pure delay on a monitor the drag is not even on.
#[test]
fn a_drag_on_one_output_does_not_hold_back_a_pan_staged_for_another() {
    let mut f = Fixture::new();
    let first = f.add_output(1, (1920, 1080));
    let second = f.add_output(2, (1920, 1080));
    let id = f.add_client();
    let surface = map_window(&mut f, id, "a", (400, 300));
    map_window(&mut f, id, "b", (400, 300));
    let fitted = window_by_app_id(&mut f, "a").unwrap();
    let other = window_by_app_id(&mut f, "b").unwrap();
    reset_view(&mut f);
    // Both viewports onto the same canvas region — the default, since every
    // output's camera starts centred on the origin — so the fit's geometry is
    // the single-output one and only the output it stages for differs.
    {
        let mut os = crate::state::output_state(&second);
        os.camera = Point::from((0.0, 0.0));
        os.zoom = 1.0;
    }
    f.state()
        .map_window(fitted.clone(), Point::from((600, 400)), false);
    f.state()
        .map_window(other.clone(), Point::from((1300, 700)), false);
    tick_until_settled(&mut f);

    // Fit against the second output, then hand the pointer back to the first.
    let want_camera = fit_target_camera(&mut f, &fitted);
    f.state().focused_output = Some(second.clone());
    f.state().fit_window(&fitted);
    f.state().focused_output = Some(first.clone());

    assert!(
        f.state()
            .try_start_gesture_move(Point::from((1400.0, 800.0)), false)
    );
    assert_eq!(
        f.state().active_output(),
        Some(first),
        "precondition: the drag leaves the fitted output inactive"
    );

    f.double_roundtrip(id);
    super::adopt_last_configure(&mut f, id, &surface);
    f.double_roundtrip(id);
    tick_until_settled(&mut f);

    let camera_target = crate::state::output_state(&second).camera_target;
    assert!(
        camera_target.is_some_and(|c| dist(c, want_camera) < 1e-6),
        "the other output's pan lands while the drag is still held, got \
         {camera_target:?} want {want_camera:?}"
    );
    end_grab(&mut f);
}

/// The take-down waits for a geometry change, not for a motion *event*. A press
/// that displaces nothing still gets motion: sub-pixel jitter truncates back to
/// the same integer canvas position, and `warp_pointer` delivers every camera
/// animation's compensating motion straight into a live grab. Ending on the
/// event alone put a plain title-bar focus click back in the business of popping
/// a running entry.
#[test]
fn a_move_motion_that_does_not_move_the_window_leaves_the_entry_alone() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    reset_view(&mut f);
    // The stand-in below has no client to close it, so it is still on the
    // stage at teardown — end off-baseline.
    f.skip_baseline_check();
    let sid = f.state().insert_suspended_for_test(
        1,
        Point::from((200, 200)),
        Size::from((300, 200)),
        "n",
        "N",
    );
    let element = standin_element(&mut f, sid);

    f.state()
        .map_window(element.clone(), Point::from((900, 200)), false);
    f.state()
        .animate_element_move_from(&element, Point::from((200, 200)), None);

    let grab_at = Point::from((1000.0, 300.0));
    assert!(f.state().try_start_gesture_move(grab_at, false));

    motion(&mut f, grab_at + Point::from((0.4, 0.4)));
    assert_eq!(
        f.state().stage.position_of(&element),
        Some(Point::from((900, 200))),
        "precondition: sub-pixel motion truncates back to the same position"
    );
    assert_eq!(
        f.state().window_animations.len(),
        1,
        "a motion that moved nothing has taken nothing away from the slide"
    );

    motion(&mut f, grab_at + Point::from((60.0, 20.0)));
    assert_eq!(
        f.state().window_animations.len(),
        0,
        "the first motion that does move it ends the slide"
    );
    end_grab(&mut f);
}

/// The resize half of the same rule: a motion too small to change the size sends
/// no configure, so there is nothing for the entry to be fighting yet.
#[test]
fn a_resize_motion_that_does_not_resize_the_window_leaves_the_entry_alone() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    map_window(&mut f, id, "a", (400, 300));
    let window = window_by_app_id(&mut f, "a").unwrap();
    reset_view(&mut f);
    f.state()
        .map_window(window.clone(), Point::from((400, 300)), false);
    tick_until_settled(&mut f);
    let serial = smithay::utils::SERIAL_COUNTER.next_serial();
    f.state().raise_and_focus(&window, serial);

    f.state()
        .execute_action(&Action::NudgeWindow(Direction::Right));
    assert_eq!(
        f.state().window_animations.len(),
        1,
        "precondition: the nudge armed a chase"
    );

    let pos = f.state().stage.position_of(&window).unwrap();
    let grab_at = Point::from((pos.x as f64 + 390.0, pos.y as f64 + 150.0));
    assert!(f.state().try_start_gesture_resize(grab_at, false));

    motion(&mut f, grab_at + Point::from((0.5, 0.0)));
    assert_eq!(
        f.state().window_animations.len(),
        1,
        "a motion that drove no new size has taken nothing away from the chase"
    );

    motion(&mut f, grab_at + Point::from((40.0, 0.0)));
    assert_eq!(
        f.state().window_animations.len(),
        0,
        "the first motion that does drive the size ends the chase"
    );
    end_grab(&mut f);
}

/// A cluster member is repositioned by every tick of the drag exactly as the
/// primary is, and unlike the primary it is not under `interactive_move` — so an
/// entry can be armed on it at any time and it has to be taken down on the same
/// terms. Left running, the member holds its frozen picture still while the drag
/// moves it, then rubber-bands when the chase releases.
#[test]
fn a_cluster_drag_ends_the_entry_a_member_fights() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    map_window(&mut f, id, "a", (400, 300));
    let window = window_by_app_id(&mut f, "a").unwrap();
    let member = StageWindow::Client(window.clone());

    // A stand-in with the client adjacent on its right → one cluster.
    let sid = f.state().insert_suspended_for_test(
        1,
        Point::from((400, 300)),
        Size::from((400, 300)),
        "s",
        "S",
    );
    let gap = f.state().config.snap_gap as i32;
    f.state()
        .map_window(member.clone(), Point::from((800 + gap, 300)), false);
    reset_view(&mut f);

    // Something displaced the member; its slide is still running.
    f.state()
        .animate_element_move_from(&member, Point::from((800 + gap, 700)), None);
    assert_eq!(
        f.state().window_animations.len(),
        1,
        "precondition: the member carries a chase"
    );

    // Cluster-drag the stand-in; the member rides the same delta.
    let grab_at = Point::from((600.0, 450.0));
    assert!(f.state().try_start_gesture_move(grab_at, true));
    assert_eq!(
        f.state().window_animations.len(),
        1,
        "installing the grab alone leaves the member's chase alone"
    );

    motion(&mut f, grab_at + Point::from((100.0, 50.0)));
    assert_eq!(
        f.state().stage.position_of(&member),
        Some(Point::from((900 + gap, 350))),
        "precondition: the drag repositioned the member"
    );
    assert_eq!(
        f.state().window_animations.len(),
        0,
        "a repositioned member is fought on the same terms as the primary"
    );
    end_grab(&mut f);
    f.state().dismiss_suspended(sid);
}

/// The resize half of the member rule: a cascade shifts its members through the
/// stage directly, so the take-down rides the shifts the cascade reports rather
/// than the primary's own configure.
#[test]
fn a_cluster_resize_ends_the_entry_a_shifted_member_fights() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    map_window(&mut f, id, "a", (400, 300));
    let window = window_by_app_id(&mut f, "a").unwrap();
    let member = StageWindow::Client(window.clone());

    let sid = f.state().insert_suspended_for_test(
        1,
        Point::from((400, 300)),
        Size::from((400, 300)),
        "s",
        "S",
    );
    let gap = f.state().config.snap_gap as i32;
    f.state()
        .map_window(member.clone(), Point::from((800 + gap, 300)), false);
    reset_view(&mut f);

    f.state()
        .animate_element_move_from(&member, Point::from((800 + gap, 700)), None);
    assert_eq!(
        f.state().window_animations.len(),
        1,
        "precondition: the member carries a chase"
    );

    // Right third of the stand-in → a right-edge resize; the member cascades.
    let grab_at = Point::from((700.0, 450.0));
    assert!(f.state().try_start_gesture_resize(grab_at, true));
    assert_eq!(
        f.state().window_animations.len(),
        1,
        "installing the grab alone leaves the member's chase alone"
    );

    motion(&mut f, grab_at + Point::from((100.0, 0.0)));
    assert_eq!(
        f.state().stage.position_of(&member),
        Some(Point::from((900 + gap, 300))),
        "precondition: the cascade shifted the member"
    );
    assert_eq!(
        f.state().window_animations.len(),
        0,
        "a shifted member is fought on the same terms as the resized primary"
    );
    end_grab(&mut f);
    f.state().dismiss_suspended(sid);
}

// A snapped fit's pushed cluster neighbours wait for the window pushing them:
// `animate_element_move_from`'s `waits_for` parks a follower on whatever entry
// it names until that entry's own start freeze releases, so the two move as one
// instead of the neighbour vacating first.

struct ParkedFollower {
    pid: ClientId,
    primary: Window,
    peid: ElementId,
    fid: ClientId,
    fsurface: ClientSurface,
    follower: Window,
    feid: ElementId,
    seed: Point<i32, Logical>,
}

/// Two real windows: a "primary" frozen on a big resize that never acks, and a
/// "follower" pushed by that resize and parked on the primary's freeze via
/// `waits_for` — the same shape `animate_cluster_shift` builds for a real
/// cluster member, without needing a real fit to construct it.
fn parked_follower(f: &mut Fixture) -> ParkedFollower {
    f.add_output(1, (1920, 1080));

    let pid = f.add_client();
    map_window(f, pid, "primary", (400, 300));
    let primary = window_by_app_id(f, "primary").unwrap();
    reset_view(f);
    f.state()
        .map_window(primary.clone(), Point::from((400, 300)), false);
    let peid = element_id(f, &primary);
    tick_until_settled(f);

    let fid = f.add_client();
    let fsurface = map_window(f, fid, "follower", (400, 300));
    let follower = window_by_app_id(f, "follower").unwrap();
    f.state()
        .map_window(follower.clone(), Point::from((900, 300)), false);
    let feid = element_id(f, &follower);
    tick_until_settled(f);

    let committed = primary.geometry().size;
    f.state().animate_window_geometry(
        &primary,
        Size::from((committed.w + 300, committed.h + 300)),
        None,
    );
    assert!(
        f.state().window_animations.start_held(peid),
        "precondition: the primary's resize froze it"
    );

    let seed = f.state().stage.position_of(&follower).unwrap();
    f.state()
        .map_window(follower.clone(), Point::from((1200, 300)), false);
    f.state()
        .animate_window_move_from(&follower, seed, Some(peid));

    ParkedFollower {
        pid,
        primary,
        peid,
        fid,
        fsurface,
        follower,
        feid,
        seed,
    }
}

/// The real wiring: a snapped fit's own pushed neighbour stays at its pre-shift
/// position while the primary is frozen on the fit's resize, and both converge
/// once the primary's client acks it.
#[test]
fn a_snapped_fits_pushed_neighbour_waits_for_the_fitting_windows_freeze() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let gap = f.state().config.snap_gap as i32;
    let id = f.add_client();

    let psurface = map_window(&mut f, id, "primary", (300, 300));
    let primary = window_by_app_id(&mut f, "primary").unwrap();
    reset_view(&mut f);
    f.state()
        .map_window(primary.clone(), Point::from((400, 300)), false);
    let peid = element_id(&mut f, &primary);
    tick_until_settled(&mut f);

    map_window(&mut f, id, "neighbour", (300, 300));
    let neighbour = window_by_app_id(&mut f, "neighbour").unwrap();
    f.state()
        .map_window(neighbour.clone(), Point::from((700 + gap, 300)), false);
    let neid = element_id(&mut f, &neighbour);
    tick_until_settled(&mut f);

    let pre_shift = f.state().stage.position_of(&neighbour).unwrap();

    f.state().fit_window_snapped(&primary);
    assert!(
        f.state().window_animations.start_held(peid),
        "the fit's resize froze the primary"
    );
    for _ in 0..10 {
        f.state().tick_window_animations(TICK);
    }
    assert_eq!(
        f.state()
            .window_animations
            .geometry_visual_rect(neid)
            .unwrap()
            .loc,
        pre_shift.to_f64(),
        "the neighbour stays put while the primary is frozen"
    );

    f.double_roundtrip(id);
    super::adopt_last_configure(&mut f, id, &psurface);
    tick_until_settled(&mut f);
    assert_eq!(
        f.state().window_animations.len(),
        0,
        "both the primary and the neighbour converged and pruned together"
    );
}

/// The same wiring for an unfit: shrinking the primary back down pushes its
/// neighbour back too, and the neighbour waits for that freeze exactly as it
/// does for a fit.
#[test]
fn an_unfits_pushed_neighbour_waits_for_the_unfitting_windows_freeze() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let gap = f.state().config.snap_gap as i32;
    let id = f.add_client();

    let psurface = map_window(&mut f, id, "primary", (300, 300));
    let primary = window_by_app_id(&mut f, "primary").unwrap();
    reset_view(&mut f);
    f.state()
        .map_window(primary.clone(), Point::from((400, 300)), false);
    let peid = element_id(&mut f, &primary);
    tick_until_settled(&mut f);

    f.state().fit_window(&primary);
    f.double_roundtrip(id);
    super::adopt_last_configure(&mut f, id, &psurface);
    tick_until_settled(&mut f);
    assert!(
        f.state().stage.is_fit(&primary),
        "precondition: the primary is fit"
    );

    let fit_pos = f.state().stage.position_of(&primary).unwrap();
    let fit_size = primary.geometry().size;
    map_window(&mut f, id, "neighbour", (300, 300));
    let neighbour = window_by_app_id(&mut f, "neighbour").unwrap();
    f.state().map_window(
        neighbour.clone(),
        Point::from((fit_pos.x + fit_size.w + gap, fit_pos.y)),
        false,
    );
    let neid = element_id(&mut f, &neighbour);
    tick_until_settled(&mut f);

    let pre_shift = f.state().stage.position_of(&neighbour).unwrap();

    f.state().unfit_window_snapped(&primary);
    assert!(
        f.state().window_animations.start_held(peid),
        "the unfit's shrink froze the primary"
    );
    for _ in 0..10 {
        f.state().tick_window_animations(TICK);
    }
    assert_eq!(
        f.state()
            .window_animations
            .geometry_visual_rect(neid)
            .unwrap()
            .loc,
        pre_shift.to_f64(),
        "the neighbour waits through the unfit's freeze too"
    );

    f.double_roundtrip(id);
    super::adopt_last_configure(&mut f, id, &psurface);
    f.double_roundtrip(id);
    tick_until_settled(&mut f);
    assert_eq!(f.state().window_animations.len(), 0);
}

/// A client that never acks releases its followers at the same degrade
/// deadline that releases it — the follower's wait resolves against
/// `frozen_at`, which answers the same "still frozen after a tick at `now`"
/// question for both.
#[test]
fn a_follower_releases_at_the_degrade_deadline_with_the_entry_it_waits_on() {
    let mut f = Fixture::new();
    let ParkedFollower {
        peid, feid, seed, ..
    } = parked_follower(&mut f);

    let base = Instant::now();
    for _ in 0..20 {
        f.state().tick_window_animations_at(TICK, base);
    }
    assert!(f.state().window_animations.start_held(peid), "still frozen");
    assert_eq!(
        f.state()
            .window_animations
            .geometry_visual_rect(feid)
            .unwrap()
            .loc,
        seed.to_f64(),
        "still parked while the primary is frozen"
    );

    let past = base + PAST_HOLD;
    f.state().tick_window_animations_at(TICK, past);
    assert!(
        !f.state().window_animations.start_held(peid),
        "the primary's budget expired"
    );
    for _ in 0..5 {
        f.state().tick_window_animations_at(TICK, past);
    }
    let after = f
        .state()
        .window_animations
        .geometry_visual_rect(feid)
        .map_or(seed.to_f64(), |r| r.loc);
    assert!(
        after.x != seed.x as f64,
        "the follower released the same tick the primary's freeze degraded"
    );
}

/// A follower that is *also* frozen on a resize of its own runs the two budgets
/// concurrently rather than end to end. Its own freeze anchors on the first tick
/// that reaches it, ahead of the wait, so it degrades alongside the entry it
/// waits on instead of starting a fresh budget once that one lets go — twice the
/// freeze, motionless, with its capture and its parked view held for all of it.
#[test]
fn a_frozen_follower_runs_both_budgets_concurrently() {
    let mut f = Fixture::new();
    let ParkedFollower {
        peid,
        feid,
        follower,
        seed,
        ..
    } = parked_follower(&mut f);

    // The follower acquires a resize of its own (which drops the wait), and is
    // then pushed by the primary's, which parks it again on top of its freeze.
    let committed = follower.geometry().size;
    f.state().animate_window_geometry(
        &follower,
        Size::from((committed.w + 300, committed.h + 300)),
        None,
    );
    assert!(
        f.state().window_animations.start_held(feid),
        "precondition: the follower froze on its own resize"
    );
    f.state()
        .animate_window_move_from(&follower, seed, Some(peid));
    assert!(
        f.state().window_animations.start_held(feid),
        "precondition: a position-only push leaves that freeze in place"
    );

    let base = Instant::now();
    f.state().tick_window_animations_at(TICK, base);
    let past = base + PAST_HOLD;
    f.state().tick_window_animations_at(TICK, past);
    assert!(
        !f.state().window_animations.start_held(peid),
        "the primary's budget expired"
    );
    assert!(
        !f.state().window_animations.start_held(feid),
        "and the follower's ran alongside it, rather than starting only now"
    );
}

/// A window still fading in that gets parked behind someone else's freeze keeps
/// fading. The wait decides where the follower is drawn, not whether it is drawn
/// at all — pinning its arrival too would leave a just-launched window entirely
/// invisible for the length of a freeze that has nothing to do with it.
#[test]
fn a_parked_follower_keeps_fading_in() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    let pid = f.add_client();
    map_window(&mut f, pid, "primary", (400, 300));
    let primary = window_by_app_id(&mut f, "primary").unwrap();
    reset_view(&mut f);
    f.state()
        .map_window(primary.clone(), Point::from((400, 300)), false);
    let peid = element_id(&mut f, &primary);
    tick_until_settled(&mut f);

    let committed = primary.geometry().size;
    f.state().animate_window_geometry(
        &primary,
        Size::from((committed.w + 300, committed.h + 300)),
        None,
    );
    assert!(
        f.state().window_animations.start_held(peid),
        "precondition: the primary's resize froze it"
    );

    // The follower is brand new — its open fade has not been drawn yet — and is
    // pushed by the primary's resize in the same breath.
    let fid = f.add_client();
    map_window(&mut f, fid, "follower", (400, 300));
    let follower = window_by_app_id(&mut f, "follower").unwrap();
    f.state()
        .map_window(follower.clone(), Point::from((900, 300)), false);
    let feid = element_id(&mut f, &follower);
    let seed = f.state().stage.position_of(&follower).unwrap();
    f.state()
        .map_window(follower.clone(), Point::from((1200, 300)), false);
    f.state()
        .animate_window_move_from(&follower, seed, Some(peid));

    let base = Instant::now();
    for _ in 0..4 {
        f.state().tick_window_animations_at(TICK, base);
    }
    assert!(
        f.state().window_animations.start_held(peid),
        "the primary is still frozen"
    );
    let visual = f
        .state()
        .window_animations
        .geometry_visual_rect(feid)
        .unwrap();
    assert_eq!(
        visual.loc,
        seed.to_f64(),
        "the follower is still parked where it was pushed from"
    );
    let size = follower.geometry().size.to_f64();
    assert!(
        f.state().animated_visual(feid, visual.loc, size).alpha > 0.0,
        "but it kept arriving while parked, rather than being held invisible"
    );
}

/// A follower named against an entry that never freezes (a same-size request,
/// which carries nothing worth waiting for) advances on the very first tick.
#[test]
fn a_follower_advances_immediately_when_the_named_entry_never_freezes() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));

    let pid = f.add_client();
    map_window(&mut f, pid, "primary", (400, 300));
    let primary = window_by_app_id(&mut f, "primary").unwrap();
    reset_view(&mut f);
    f.state()
        .map_window(primary.clone(), Point::from((400, 300)), false);
    let peid = element_id(&mut f, &primary);
    tick_until_settled(&mut f);

    let committed = primary.geometry().size;
    f.state().animate_window_geometry(&primary, committed, None);
    assert!(
        !f.state().window_animations.start_held(peid),
        "an equal-size request never freezes"
    );

    let fid = f.add_client();
    map_window(&mut f, fid, "follower", (400, 300));
    let follower = window_by_app_id(&mut f, "follower").unwrap();
    f.state()
        .map_window(follower.clone(), Point::from((900, 300)), false);
    let feid = element_id(&mut f, &follower);
    tick_until_settled(&mut f);

    let seed = f.state().stage.position_of(&follower).unwrap();
    f.state()
        .map_window(follower.clone(), Point::from((1200, 300)), false);
    f.state()
        .animate_window_move_from(&follower, seed, Some(peid));

    f.state().tick_window_animations(TICK);
    let after = f
        .state()
        .window_animations
        .geometry_visual_rect(feid)
        .unwrap()
        .loc;
    assert!(
        after.x > seed.x as f64,
        "nothing to wait on, so the follower moved on the very first tick"
    );
}

/// Cancelling the entry a follower waits on releases the follower too: it has
/// nothing left to resolve against, so it converges normally on the very next
/// tick instead of stalling for the rest of the (now nonexistent) budget.
#[test]
fn cancelling_the_entry_a_follower_waits_on_releases_it() {
    let mut f = Fixture::new();
    let ParkedFollower { primary, .. } = parked_follower(&mut f);

    f.state().cancel_window_animation(&primary);

    // Would spin to a panic if the follower were still parked on an entry that
    // no longer exists and can never resolve.
    tick_until_settled(&mut f);
    assert_eq!(f.state().window_animations.len(), 0);
}

/// A second freeze landing on the entry a follower waits on (a second fit
/// pressed inside the first one's budget) must not break the follower away
/// early. `frozen_at` asks the live state fresh every tick, so a fresh freeze
/// holds the follower exactly like the first one did.
#[test]
fn a_follower_stays_parked_through_a_second_freeze_on_the_entry_it_waits_on() {
    let mut f = Fixture::new();
    let ParkedFollower {
        primary,
        peid,
        feid,
        seed,
        ..
    } = parked_follower(&mut f);

    for _ in 0..5 {
        f.state().tick_window_animations(TICK);
    }
    assert_eq!(
        f.state()
            .window_animations
            .geometry_visual_rect(feid)
            .unwrap()
            .loc,
        seed.to_f64(),
        "parked through the first freeze"
    );

    let committed = primary.geometry().size;
    f.state().animate_window_geometry(
        &primary,
        Size::from((committed.w + 400, committed.h + 400)),
        None,
    );
    assert!(
        f.state().window_animations.start_held(peid),
        "refroze on the second request"
    );

    for _ in 0..5 {
        f.state().tick_window_animations(TICK);
    }
    assert_eq!(
        f.state()
            .window_animations
            .geometry_visual_rect(feid)
            .unwrap()
            .loc,
        seed.to_f64(),
        "still parked — no early break-away across the second freeze"
    );
}

/// A hide-to-tray remap writes an open entry straight over the primary's
/// frozen geometry entry — there is no remove site to hang the cleanup on — and
/// a follower waiting on it must not stall for the rest of the budget: the wait
/// stops resolving the moment the named entry is no longer a frozen `Geometry`.
#[test]
fn a_follower_proceeds_once_start_open_overwrites_the_entry_it_waits_on() {
    let mut f = Fixture::new();
    let ParkedFollower {
        primary,
        feid,
        seed,
        ..
    } = parked_follower(&mut f);

    f.state().start_window_open_animation(&primary);
    f.state().tick_window_animations(TICK);
    let after = f
        .state()
        .window_animations
        .geometry_visual_rect(feid)
        .unwrap()
        .loc;
    assert!(
        after.x > seed.x as f64,
        "the follower released the moment the wait stopped resolving"
    );

    tick_until_settled(&mut f);
    assert_eq!(
        f.state().window_animations.len(),
        0,
        "both the remapped primary and the follower converged"
    );
}

/// A client that dies without a clean unmap leaves its window animation entry
/// for the dead-id sweep (`retain_ids`) to collect, not an eager `remove` — a
/// follower waiting on it must not stall until that sweep runs.
#[test]
fn a_follower_proceeds_once_a_dead_clients_entry_is_swept() {
    let mut f = Fixture::new();
    let ParkedFollower {
        pid, feid, seed, ..
    } = parked_follower(&mut f);

    f.kill_client(pid);
    f.pump(3);

    f.state().tick_window_animations(TICK);
    let after = f
        .state()
        .window_animations
        .geometry_visual_rect(feid)
        .unwrap()
        .loc;
    assert!(
        after.x > seed.x as f64,
        "the follower released once the dead primary's entry was swept"
    );
}

/// A follower that acquires its own resize stops waiting on the primary — it is
/// now a window others can be pushed by. Proven by resolving the follower's own
/// freeze and letting it converge while the primary stays frozen forever
/// (never acked): if the wait had survived, the follower could never move.
#[test]
fn a_follower_that_acquires_its_own_resize_stops_waiting() {
    let mut f = Fixture::new();
    let ParkedFollower {
        peid,
        fid,
        fsurface,
        follower,
        feid,
        seed,
        ..
    } = parked_follower(&mut f);

    for _ in 0..5 {
        f.state().tick_window_animations(TICK);
    }
    assert_eq!(
        f.state()
            .window_animations
            .geometry_visual_rect(feid)
            .unwrap()
            .loc,
        seed.to_f64(),
        "parked before it has a request of its own"
    );

    let f_committed = follower.geometry().size;
    let f_bigger = Size::from((f_committed.w + 300, f_committed.h + 300));
    f.state().animate_window_geometry(&follower, f_bigger, None);
    assert!(
        f.state().window_animations.start_held(feid),
        "now frozen on its own account"
    );

    let w = f.client(fid).window(&fsurface);
    w.set_size(f_bigger.w as u16, f_bigger.h as u16);
    w.attach_new_buffer();
    w.ack_last_and_commit();
    f.double_roundtrip(fid);

    for _ in 0..200 {
        f.state().tick_window_animations(TICK);
    }
    assert!(
        f.state()
            .window_animations
            .geometry_visual_rect(feid)
            .is_none(),
        "converged on its own resize, independent of the primary"
    );
    assert!(
        f.state().window_animations.start_held(peid),
        "sanity: the primary is still frozen — it was never acked"
    );
}

/// A follower nudged mid-wait (a position-only retarget naming no one) keeps
/// waiting: only a request-carrying retarget on the follower's own entry clears
/// `waits_for`, so the neighbour that is still being pushed keeps behaving like
/// one.
#[test]
fn a_follower_nudged_mid_wait_stays_parked() {
    let mut f = Fixture::new();
    let ParkedFollower {
        follower,
        feid,
        seed,
        ..
    } = parked_follower(&mut f);

    for _ in 0..3 {
        f.state().tick_window_animations(TICK);
    }
    assert_eq!(
        f.state()
            .window_animations
            .geometry_visual_rect(feid)
            .unwrap()
            .loc,
        seed.to_f64()
    );

    f.state()
        .map_window(follower.clone(), Point::from((1500, 300)), false);
    f.state().animate_window_move_from(&follower, seed, None);

    for _ in 0..5 {
        f.state().tick_window_animations(TICK);
    }
    assert_eq!(
        f.state()
            .window_animations
            .geometry_visual_rect(feid)
            .unwrap()
            .loc,
        seed.to_f64(),
        "still parked — the nudge named nobody, so the existing wait survives"
    );
}

// Suspended stand-ins are cluster members too: a stand-in's entry is
// position-only and element-based rather than window-based, but it waits,
// slides, culls and gets swept on dismiss exactly like a client member.

/// A snapped fit slides a stand-in cluster member exactly like a client
/// member — parked while the primary is frozen, then landing on the stage
/// position once the shift's own freeze releases.
#[test]
fn a_snapped_fit_slides_a_stand_in_cluster_member_and_lands_it_at_its_stage_position() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let gap = f.state().config.snap_gap as i32;
    let id = f.add_client();

    let psurface = map_window(&mut f, id, "primary", (300, 300));
    let primary = window_by_app_id(&mut f, "primary").unwrap();
    reset_view(&mut f);
    // The stand-in below has no client to close it, so it is still on the
    // stage at teardown — end off-baseline.
    f.skip_baseline_check();
    f.state()
        .map_window(primary.clone(), Point::from((400, 300)), false);
    let peid = element_id(&mut f, &primary);
    tick_until_settled(&mut f);

    let sid = f.state().insert_suspended_for_test(
        1,
        Point::from((700 + gap, 300)),
        Size::from((300, 300)),
        "n",
        "N",
    );
    let element = standin_element(&mut f, sid);
    let neid = f.state().stage.id_of(&element).unwrap();
    let pre_shift = f.state().stage.position_of(&element).unwrap();

    f.state().fit_window_snapped(&primary);
    assert!(
        f.state().window_animations.start_held(peid),
        "the fit froze the primary"
    );
    for _ in 0..10 {
        f.state().tick_window_animations(TICK);
    }
    assert_eq!(
        f.state()
            .window_animations
            .geometry_visual_rect(neid)
            .unwrap()
            .loc,
        pre_shift.to_f64(),
        "the stand-in waits for the primary's freeze exactly like a client member"
    );

    f.double_roundtrip(id);
    super::adopt_last_configure(&mut f, id, &psurface);
    tick_until_settled(&mut f);

    let final_pos = f.state().stage.position_of(&element).unwrap();
    assert_ne!(
        final_pos, pre_shift,
        "precondition: the shift actually pushed it"
    );
    assert!(
        f.state()
            .window_animations
            .geometry_visual_rect(neid)
            .is_none(),
        "the slide converged and pruned, landing on the stage position"
    );
}

/// A resize that also moves can put the live rect off every viewport while the
/// picture an animation is still drawing stays on it — `window_cull_rect`
/// merges the two so the frame composer never culls a stand-in mid-slide, same
/// as it already does for a client.
#[test]
fn a_stand_ins_slide_is_not_culled_when_its_target_leaves_every_viewport() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    reset_view(&mut f);
    // The stand-in below has no client to close it, so it is still on the
    // stage at teardown — end off-baseline.
    f.skip_baseline_check();
    let sid = f.state().insert_suspended_for_test(
        1,
        Point::from((200, 200)),
        Size::from((300, 200)),
        "n",
        "N",
    );
    let element = standin_element(&mut f, sid);
    let id = f.state().stage.id_of(&element).unwrap();

    let seed = Point::from((200, 200));
    f.state()
        .map_window(element.clone(), Point::from((60000, 60000)), false);
    f.state().animate_element_move_from(&element, seed, None);

    let bbox = Rectangle::new(Point::from((60000, 60000)), Size::from((300, 200)));
    assert!(
        !f.state().canvas_rect_drawable(bbox),
        "precondition: the live target has left every viewport"
    );
    let culled = f.state().window_cull_rect(Some(id), bbox);
    assert!(
        f.state().canvas_rect_drawable(culled),
        "but the picture is still on screen mid-slide, so it must still be drawn"
    );
}

/// The rect a dismiss fades from is where the stand-in's picture is, not where
/// its slide was headed. Reading the destination both freezes the departing
/// chrome at the far end and — when that end is off every viewport — judges the
/// fade undrawable and skips it, losing an animation whose picture is on screen.
#[test]
fn a_dismissed_stand_in_departs_from_its_mid_slide_picture_not_its_destination() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    reset_view(&mut f);
    // The stand-in below has no client to close it, so it is still on the
    // stage at teardown — end off-baseline.
    f.skip_baseline_check();
    let sid = f.state().insert_suspended_for_test(
        1,
        Point::from((200, 200)),
        Size::from((300, 200)),
        "n",
        "N",
    );
    let element = standin_element(&mut f, sid);

    // Slide across the viewport first and stop partway. Reading at t=0 proves
    // nothing: the seed is still exactly the from-rect on the first frame, so
    // the picture and either end coincide.
    let start = Point::from((200, 200));
    f.state()
        .map_window(element.clone(), Point::from((1500, 200)), false);
    f.state().animate_element_move_from(&element, start, None);
    for _ in 0..3 {
        f.state().tick_window_animations(TICK);
    }

    let midway = f
        .state()
        .departing_standin_rect(&element)
        .expect("the stand-in is still on the stage")
        .loc;
    assert!(
        midway.x > start.x as f64 && midway.x < 1500.0,
        "the departing rect is the leg's live picture, between where the slide \
         began and where it is headed: {midway:?}"
    );

    // Now re-aim it off every viewport, which is where reading the destination
    // costs the animation outright rather than merely misplacing it.
    f.state()
        .map_window(element.clone(), Point::from((60000, 60000)), false);
    f.state()
        .animate_element_move_from(&element, Point::from((1500, 200)), None);
    let destination = Rectangle::new(Point::from((60000, 60000)), Size::from((300, 200)));
    assert!(
        !f.state().canvas_rect_drawable(destination),
        "precondition: the slide is headed off every viewport"
    );

    let departing = f
        .state()
        .departing_standin_rect(&element)
        .expect("the stand-in is still on the stage")
        .loc;
    assert_eq!(
        departing, midway,
        "the re-aim carried the picture forward instead of jumping it to either end"
    );
    assert!(
        f.state()
            .canvas_rect_drawable(Rectangle::new(departing.to_i32_round(), (300, 200).into())),
        "so the dismiss fades it instead of skipping it as undrawable"
    );
}

/// Dismissing a stand-in mid-slide drops its window-animation entry
/// immediately, rather than leaving the next tick's dead-id sweep to reap it a
/// frame late.
#[test]
fn dismissing_a_stand_in_mid_slide_drops_its_entry() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    reset_view(&mut f);
    let sid = f.state().insert_suspended_for_test(
        1,
        Point::from((200, 200)),
        Size::from((300, 200)),
        "n",
        "N",
    );
    let element = standin_element(&mut f, sid);

    f.state()
        .map_window(element.clone(), Point::from((900, 200)), false);
    f.state()
        .animate_element_move_from(&element, Point::from((200, 200)), None);
    assert_eq!(
        f.state().window_animations.len(),
        1,
        "the slide armed an entry"
    );

    f.state().dismiss_suspended(sid);

    assert_eq!(
        f.state().window_animations.len(),
        0,
        "dismiss purged the entry, not just the stage slot"
    );
}

/// A stand-in mid-interactive-drag is guarded exactly like a client window:
/// the shared `interactive_move` set suppresses any animation start on it,
/// whether it was armed by a move grab's install or a resize grab's — both
/// arm the same set.
#[test]
fn no_entry_starts_on_a_stand_in_under_an_interactive_grab() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    reset_view(&mut f);
    // The stand-in below has no client to close it, so it is still on the
    // stage at teardown — end off-baseline.
    f.skip_baseline_check();
    let sid = f.state().insert_suspended_for_test(
        1,
        Point::from((200, 200)),
        Size::from((300, 200)),
        "n",
        "N",
    );
    let element = standin_element(&mut f, sid);

    f.state().arm_interactive_move(&sid);
    f.state()
        .map_window(element.clone(), Point::from((900, 200)), false);
    f.state()
        .animate_element_move_from(&element, Point::from((200, 200)), None);
    assert_eq!(
        f.state().window_animations.len(),
        0,
        "the grab guard suppressed the start, same as it does for a client"
    );
    f.state().disarm_interactive_move(&sid);
}

// Pin/unpin animates the canvas <-> screen flip it makes to the on-screen rect,
// instead of jumping it in one frame at zoom != 1.

/// Pinning at zoom 0.5 draws the pre-toggle on-screen rect on the very first
/// frame — a zero jump — and grows from there to the pin site at full size.
#[test]
fn pinning_at_zoom_half_grows_from_the_pre_toggle_rect_to_the_pin_site() {
    let mut f = Fixture::new();
    let output = f.add_output(1, (1920, 1080));
    let id = f.add_client();
    map_window(&mut f, id, "a", (400, 300));
    let window = window_by_app_id(&mut f, "a").unwrap();
    reset_view(&mut f);
    f.state().with_output_state(|os| os.zoom = 0.5);
    f.state().update_output_from_camera();
    tick_until_settled(&mut f);
    let eid = element_id(&mut f, &window);
    let serial = smithay::utils::SERIAL_COUNTER.next_serial();
    f.state().raise_and_focus(&window, serial);

    let (camera, zoom) = f
        .state()
        .with_output_state(|os| (os.camera, os.zoom))
        .unwrap();
    let canvas_loc = f.state().stage.position_of(&window).unwrap().to_f64();
    let canvas_size = window.geometry().size.to_f64();
    let pre_toggle = Rectangle::new(
        Point::from((
            (canvas_loc.x - camera.x) * zoom,
            (canvas_loc.y - camera.y) * zoom,
        )),
        Size::from((canvas_size.w * zoom, canvas_size.h * zoom)),
    );

    f.state().execute_action(&Action::TogglePinToScreen);
    assert!(f.state().is_pinned(&window), "the window pinned");
    assert!(
        !f.state().window_animations.start_held(eid),
        "a pin entry carries no request, so it never freezes"
    );
    let first_frame = f
        .state()
        .window_animations
        .geometry_visual_rect(eid)
        .unwrap();
    assert_eq!(
        (first_frame.loc, first_frame.size),
        (pre_toggle.loc, pre_toggle.size),
        "the first frame after pinning draws exactly the pre-toggle on-screen rect"
    );
    // The numbers above are identical in either space at this instant, so the
    // rect alone would pass with the seed planted in the wrong one and every
    // later frame drawn through the camera transform instead of the pin's.
    assert_eq!(
        f.state().window_animations.geometry_space(eid),
        Some(AnimSpace::Screen(output.name())),
        "the pinned chase runs in its output's screen space"
    );

    tick_until_settled(&mut f);
    assert!(
        f.state()
            .window_animations
            .geometry_visual_rect(eid)
            .is_none(),
        "the entry converged on the pin site at full size and pruned"
    );
}

/// Unpinning at zoom 0.5, in reverse: seeds at the pre-toggle rect expressed in
/// canvas coordinates, and shrinks from there to the live canvas rect.
#[test]
fn unpinning_at_zoom_half_shrinks_from_the_pin_site_to_the_canvas_rect() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    map_window(&mut f, id, "a", (400, 300));
    let window = window_by_app_id(&mut f, "a").unwrap();
    reset_view(&mut f);
    f.state().with_output_state(|os| os.zoom = 0.5);
    f.state().update_output_from_camera();
    tick_until_settled(&mut f);
    let eid = element_id(&mut f, &window);
    let serial = smithay::utils::SERIAL_COUNTER.next_serial();
    f.state().raise_and_focus(&window, serial);

    f.state().execute_action(&Action::TogglePinToScreen);
    assert!(f.state().is_pinned(&window), "the window pinned");
    tick_until_settled(&mut f);

    let site = f.state().stage.pin_of(&window).cloned().unwrap();
    let (camera, zoom) = f
        .state()
        .with_output_state(|os| (os.camera, os.zoom))
        .unwrap();
    let size = window.geometry().size.to_f64();
    let pre_toggle = Rectangle::new(
        Point::from((
            camera.x + site.screen_pos.x as f64 / zoom,
            camera.y + site.screen_pos.y as f64 / zoom,
        )),
        Size::from((size.w / zoom, size.h / zoom)),
    );

    f.state().execute_action(&Action::TogglePinToScreen);
    assert!(!f.state().is_pinned(&window), "the window unpinned");
    assert!(
        !f.state().window_animations.start_held(eid),
        "a pin entry carries no request, so it never freezes"
    );
    let first_frame = f
        .state()
        .window_animations
        .geometry_visual_rect(eid)
        .unwrap();
    assert_eq!(
        (first_frame.loc, first_frame.size),
        (pre_toggle.loc, pre_toggle.size),
        "the first frame after unpinning draws the pre-toggle rect in canvas space"
    );
    assert_eq!(
        f.state().window_animations.geometry_space(eid),
        Some(AnimSpace::Canvas),
        "the unpinned chase runs through the camera again, not the pin's screen space"
    );

    tick_until_settled(&mut f);
    assert!(
        f.state()
            .window_animations
            .geometry_visual_rect(eid)
            .is_none(),
        "the entry converged on the live canvas rect at full size and pruned"
    );
}

/// At zoom 1 the pin site and the pre-toggle canvas rect coincide exactly, so
/// the toggle is visually a no-op — no special-cased skip, just seed == target.
#[test]
fn pin_toggle_at_zoom_one_is_visually_a_no_op() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    map_window(&mut f, id, "a", (400, 300));
    let window = window_by_app_id(&mut f, "a").unwrap();
    reset_view(&mut f);
    tick_until_settled(&mut f);
    let serial = smithay::utils::SERIAL_COUNTER.next_serial();
    f.state().raise_and_focus(&window, serial);

    let canvas_loc = f.state().stage.position_of(&window).unwrap().to_f64();

    f.state().execute_action(&Action::TogglePinToScreen);
    assert!(f.state().is_pinned(&window), "the window pinned");
    let site = f.state().stage.pin_of(&window).cloned().unwrap();
    assert_eq!(
        site.screen_pos.to_f64(),
        canvas_loc,
        "at zoom 1 the pin site is exactly the pre-toggle canvas position"
    );

    tick_until_settled(&mut f);
    assert_eq!(
        f.state().window_animations.len(),
        0,
        "seed == target at zoom 1, so there was nothing to animate"
    );
}

/// A pin toggled while the window is frozen mid-fit cancels the fit's frozen
/// entry (`cancelling_a_frozen_fit_drops_its_parked_pan`) and starts a pin
/// entry that carries no request of its own. When the fit's now-orphaned
/// configure is acked late, the pin entry has nothing outstanding to resolve —
/// it just reads the window's new (bigger) live size on its next tick, so the
/// drawn size jumps to it outright rather than freezing a second time.
#[test]
fn a_late_fit_ack_after_a_pin_toggle_resizes_the_pin_entry_live() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let surface = map_window(&mut f, id, "a", (400, 300));
    let window = window_by_app_id(&mut f, "a").unwrap();
    reset_view(&mut f);
    f.state()
        .map_window(window.clone(), Point::from((600, 400)), false);
    let eid = element_id(&mut f, &window);
    tick_until_settled(&mut f);

    f.state().fit_window(&window);
    assert!(
        f.state().window_animations.start_held(eid),
        "the fit froze the window"
    );
    let pre_fit_size = window.geometry().size;

    f.state().execute_action(&Action::TogglePinToScreen);
    assert!(f.state().is_pinned(&window), "the toggle pinned the window");
    assert!(
        !f.state().window_animations.start_held(eid),
        "a pin entry carries no request, so it never freezes"
    );

    f.double_roundtrip(id);
    super::adopt_last_configure(&mut f, id, &surface);
    f.double_roundtrip(id);
    assert_ne!(
        window.geometry().size,
        pre_fit_size,
        "precondition: the late ack actually changed the live size"
    );

    tick_until_settled(&mut f);
    assert_eq!(
        f.state().window_animations.len(),
        0,
        "the pin entry's target flipped to the new live size and converged \
         there, rather than stalling on the pre-fit one"
    );
}

// A window that opens straight into fullscreen (or fit) fades in already at
// its destination rect, instead of popping in at the placement rect and then
// growing — see `map_straight_into_fullscreen` / `map_straight_into_fit`.

/// Map a window that requests fullscreen before its first commit — the
/// deferred `pending_fullscreen` path a game or xwayland-satellite client
/// takes. `first_size` is what the client's first (still un-fullscreened)
/// buffer commits at; the compositor's own fullscreen configure follows in the
/// same map commit, left un-acked. Returns the surface.
fn map_straight_into_fullscreen(
    f: &mut Fixture,
    id: ClientId,
    app_id: &str,
    first_size: (u16, u16),
) -> ClientSurface {
    let window = f.client(id).create_window();
    let surface = window.surface.clone();
    window.set_app_id(app_id);
    window.set_fullscreen(None);
    window.commit();
    f.roundtrip(id);

    let w = f.client(id).window(&surface);
    w.set_size(first_size.0, first_size.1);
    w.attach_new_buffer();
    w.ack_last_and_commit();
    f.double_roundtrip(id);
    surface
}

/// As [`map_straight_into_fullscreen`], but the client self-maximizes before
/// its first commit instead — the deferred `pending_fit` path.
fn map_straight_into_fit(
    f: &mut Fixture,
    id: ClientId,
    app_id: &str,
    first_size: (u16, u16),
) -> ClientSurface {
    let window = f.client(id).create_window();
    let surface = window.surface.clone();
    window.set_app_id(app_id);
    window.set_maximized();
    window.commit();
    f.roundtrip(id);

    let w = f.client(id).window(&surface);
    w.set_size(first_size.0, first_size.1);
    w.attach_new_buffer();
    w.ack_last_and_commit();
    f.double_roundtrip(id);
    surface
}

/// The scene behind an open-into-fullscreen fade is never culled. A design
/// that dropped the geometry entry in favour of a bare `fullscreen = true`
/// stage flag would report the output visually fullscreen from the instant the
/// stage wrote membership — background freed, every other window culled —
/// while the entering window itself drew at alpha 0 for the whole freeze.
/// Extends `output_is_visually_fullscreen_only_after_the_entry_finishes` to the
/// map-time path.
#[test]
fn a_window_that_opens_into_fullscreen_never_reports_the_output_visually_fullscreen_early() {
    let mut f = Fixture::new();
    let output = f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let surface = map_straight_into_fullscreen(&mut f, id, "fs", (400, 300));
    let window = window_by_app_id(&mut f, "fs").unwrap();
    let eid = element_id(&mut f, &window);

    assert!(
        f.state().window_animations.start_held(eid),
        "frozen, waiting for the fullscreen-sized commit"
    );
    let loc = f.state().stage.position_of(&window).unwrap().to_f64();
    let size = window.geometry().size.to_f64();
    assert_eq!(
        f.state().animated_visual(eid, loc, size).alpha,
        0.0,
        "nothing is drawn while the fade waits"
    );
    assert!(
        !f.state().is_output_visually_fullscreen(&output),
        "the output is not visually fullscreen while the fade waits"
    );

    super::adopt_last_configure(&mut f, id, &surface);
    assert!(
        !f.state().is_output_visually_fullscreen(&output),
        "nor right after the freeze releases, mid-fade"
    );

    tick_until_settled(&mut f);
    assert!(
        f.state().is_output_visually_fullscreen(&output),
        "only once the fade lands"
    );
    let final_loc = f.state().stage.position_of(&window).unwrap().to_f64();
    let final_size = window.geometry().size.to_f64();
    assert_eq!(
        f.state().animated_visual(eid, final_loc, final_size).alpha,
        1.0,
        "and it fully faded in"
    );

    f.state().exit_fullscreen_on(&output);
}

/// As `the_scene_keeps_its_view_until_the_fullscreen_entry_lands`, for a window
/// that maps straight into fullscreen: the pre-map scene keeps rendering behind
/// the fade for the whole freeze, and only follows the zoom-1 park once the
/// entry — by then covering the output — lands.
#[test]
fn the_scene_keeps_its_pre_map_view_through_an_open_into_fullscreen_fade() {
    let mut f = Fixture::new();
    let output = f.add_output(1, (1920, 1080));
    f.skip_baseline_check();
    f.state().with_output_state(|os| {
        os.camera = Point::from((40.0, 25.0));
        os.zoom = 0.5;
    });
    f.state().update_output_from_camera();
    let pre = f
        .state()
        .with_output_state(|os| (os.camera, os.zoom))
        .unwrap();
    assert_eq!(
        f.state().world_view(&output),
        pre,
        "with nothing mapped yet the scene is on the live viewport"
    );

    let id = f.add_client();
    let surface = map_straight_into_fullscreen(&mut f, id, "fs", (400, 300));

    let parked = f
        .state()
        .with_output_state(|os| (os.camera, os.zoom))
        .unwrap();
    assert_eq!(parked.1, 1.0, "the viewport parked at zoom 1");
    assert_ne!(parked, pre, "the park moved the live viewport");
    assert_eq!(
        f.state().world_view(&output),
        pre,
        "the scene stays on the pre-map view for the whole freeze"
    );

    super::adopt_last_configure(&mut f, id, &surface);
    assert_eq!(
        f.state().world_view(&output),
        pre,
        "and still, right as the fade starts running"
    );
    tick_until_settled(&mut f);
    assert_eq!(
        f.state().world_view(&output),
        parked,
        "the scene follows the park only once the entry lands"
    );

    f.state().exit_fullscreen_on(&output);
}

/// No compositor chrome is drawn over an open-into-fullscreen fade: there was
/// never a windowed picture to hand the border/shadow ramp over *from*, so
/// `chrome_ramp` must stay bare for the whole leg instead of fading a ring in
/// and back out around the growing window.
#[test]
fn no_chrome_is_drawn_over_an_open_into_fullscreen_fade() {
    let mut f = Fixture::new();
    let output = f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let surface = map_straight_into_fullscreen(&mut f, id, "fs", (400, 300));
    let window = window_by_app_id(&mut f, "fs").unwrap();

    assert_eq!(chrome_alpha(&mut f, &window), 0.0, "bare while frozen");

    super::adopt_last_configure(&mut f, id, &surface);
    for _ in 0..10 {
        f.state().tick_window_animations(TICK);
        assert_eq!(
            chrome_alpha(&mut f, &window),
            0.0,
            "and bare through every tick of the fade — no ring or shadow ramp"
        );
    }
    tick_until_settled(&mut f);
    assert_eq!(chrome_alpha(&mut f, &window), 0.0, "and once it lands");

    f.state().exit_fullscreen_on(&output);
}

/// A window that maps already at output size gets no freeze at all — the
/// request equals what is already committed — so the fade runs immediately
/// instead of sitting invisible for a resize that will never arrive.
#[test]
fn a_window_mapped_already_at_output_size_fades_in_without_freezing() {
    let mut f = Fixture::new();
    let output = f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let _surface = map_straight_into_fullscreen(&mut f, id, "fs", (1920, 1080));
    let window = window_by_app_id(&mut f, "fs").unwrap();
    let eid = element_id(&mut f, &window);

    assert!(
        !f.state().window_animations.start_held(eid),
        "the request matched what was already committed, so nothing was worth \
         freezing"
    );
    // Not held, so — unlike a frozen entry — this one actually advances.
    f.state().tick_window_animations(TICK);
    let loc = f.state().stage.position_of(&window).unwrap().to_f64();
    let size = window.geometry().size.to_f64();
    assert!(
        f.state().animated_visual(eid, loc, size).alpha > 0.0,
        "the fade is already running rather than sitting invisible"
    );

    tick_until_settled(&mut f);
    assert!(f.state().is_output_visually_fullscreen(&output));
    f.state().exit_fullscreen_on(&output);
}

/// A client that self-maximizes during map takes the same shortcut for fit:
/// frozen and invisible while the fade waits, seeded already at the fit rect
/// (never the placement rect), then fully faded in once it lands.
#[test]
fn a_client_that_self_maximizes_during_map_fades_in_at_the_fit_rect() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let surface = map_straight_into_fit(&mut f, id, "a", (400, 300));
    let window = window_by_app_id(&mut f, "a").unwrap();
    let eid = element_id(&mut f, &window);

    assert!(
        f.state().window_animations.start_held(eid),
        "the fit's resize froze the window"
    );
    let seeded = f
        .state()
        .window_animations
        .geometry_visual_rect(eid)
        .unwrap();
    let fit_loc = f.state().stage.position_of(&window).unwrap().to_f64();
    assert_eq!(
        seeded.loc, fit_loc,
        "seeded already at the fit's destination, not the placement rect"
    );
    let loc = f.state().stage.position_of(&window).unwrap().to_f64();
    let size = window.geometry().size.to_f64();
    assert_eq!(
        f.state().animated_visual(eid, loc, size).alpha,
        0.0,
        "nothing is drawn while the fade waits"
    );

    super::adopt_last_configure(&mut f, id, &surface);
    tick_until_settled(&mut f);
    assert!(f.state().stage.is_fit(&window));
    let final_loc = f.state().stage.position_of(&window).unwrap().to_f64();
    let final_size = window.geometry().size.to_f64();
    let settled = f.state().animated_visual(eid, final_loc, final_size);
    assert_eq!(settled.alpha, 1.0, "fully faded in");
    assert_eq!(settled.loc, final_loc, "at the fit rect");
}

/// A client that never redraws still fades in eventually: the freeze degrades
/// at `MAX_START_HOLD` and the leg starts running with stale (capped) content,
/// same as any other compositor-initiated resize.
#[test]
fn an_open_into_fullscreen_fade_degrades_at_the_start_hold_deadline_if_never_acked() {
    let mut f = Fixture::new();
    let output = f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let _surface = map_straight_into_fullscreen(&mut f, id, "fs", (400, 300));
    let window = window_by_app_id(&mut f, "fs").unwrap();
    let eid = element_id(&mut f, &window);

    let base = Instant::now();
    for _ in 0..10 {
        f.state().tick_window_animations_at(TICK, base);
    }
    assert!(
        f.state().window_animations.start_held(eid),
        "still frozen, nothing acked"
    );
    let loc = f.state().stage.position_of(&window).unwrap().to_f64();
    let size = window.geometry().size.to_f64();
    assert_eq!(
        f.state().animated_visual(eid, loc, size).alpha,
        0.0,
        "still invisible"
    );

    let past = base + PAST_HOLD;
    for _ in 0..30 {
        f.state().tick_window_animations_at(TICK, past);
    }
    assert!(
        !f.state().window_animations.start_held(eid),
        "the budget expired"
    );
    assert!(
        f.state().animated_visual(eid, loc, size).alpha > 0.0,
        "the degrade let the fade start running with stale content"
    );

    f.state().exit_fullscreen_on(&output);
}

/// A window fullscreened after its open fade has already started (progress >
/// 0) has been seen at its placement rect, so it keeps the ordinary grow leg
/// from there rather than being put straight at the fullscreen rect. The fade
/// itself rides along — an arrival interrupted partway is still an arrival, and
/// dropping it would pop the window to full opacity.
#[test]
fn a_window_fullscreened_mid_open_keeps_its_grow_leg() {
    let mut f = Fixture::new();
    let output = f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let surface = map_window(&mut f, id, "fs", (400, 300));
    let window = window_by_app_id(&mut f, "fs").unwrap();
    reset_view(&mut f);
    let eid = element_id(&mut f, &window);

    for _ in 0..5 {
        f.state().tick_window_animations(TICK);
    }
    let loc = f.state().stage.position_of(&window).unwrap().to_f64();
    let size = window.geometry().size.to_f64();
    let mid_open = f.state().animated_visual(eid, loc, size);
    assert!(
        mid_open.alpha > 0.0 && mid_open.alpha < 1.0,
        "precondition: partway through the open fade, not fresh off the map"
    );

    f.client(id).window(&surface).set_fullscreen(None);
    f.double_roundtrip(id);
    assert!(
        f.state().window_animations.has_open_fade(eid),
        "the fade in flight carried over onto the fullscreen chase"
    );
    let seed = f
        .state()
        .window_animations
        .geometry_visual_rect(eid)
        .expect("the fullscreen enter armed a chase");
    assert!(
        seed.size.w < 1920.0 && seed.size.h < 1080.0,
        "the leg still grows from the windowed rect, not the fullscreen one ({seed:?})"
    );

    super::adopt_last_configure(&mut f, id, &surface);
    tick_until_settled(&mut f);
    assert!(f.state().is_output_visually_fullscreen(&output));
    f.state().exit_fullscreen_on(&output);
}

/// A fullscreen request landing on a window whose open fade has already been
/// drawn must not strip its chrome. The freeze is holding a picture the user has
/// seen — bar, border and shadow, at the pre-fullscreen rect — so the chrome
/// leaves across the grow like any other fullscreen enter: in one direction, and
/// without a step when the fade lands ahead of the leg.
#[test]
fn a_fullscreen_enter_over_a_drawn_open_fade_hands_its_chrome_over() {
    let mut f = Fixture::new();
    let output = f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let surface = map_window(&mut f, id, "fs", (400, 300));
    let window = window_by_app_id(&mut f, "fs").unwrap();
    reset_view(&mut f);
    let eid = element_id(&mut f, &window);

    for _ in 0..5 {
        f.state().tick_window_animations(TICK);
    }
    let loc = f.state().stage.position_of(&window).unwrap().to_f64();
    let size = window.geometry().size.to_f64();
    assert!(
        f.state().animated_visual(eid, loc, size).alpha < 1.0,
        "precondition: the window has been on screen, partway through its fade"
    );

    f.client(id).window(&surface).set_fullscreen(None);
    f.double_roundtrip(id);
    assert!(
        f.state().window_animations.has_open_fade(eid),
        "precondition: the fade rode onto the fullscreen chase"
    );
    assert_eq!(
        chrome_alpha(&mut f, &window),
        1.0,
        "the frozen picture is the windowed one the user was shown, chrome and all"
    );

    super::adopt_last_configure(&mut f, id, &surface);
    let mut previous = chrome_alpha(&mut f, &window);
    for _ in 0..MAX_TICKS {
        if !f.state().window_animations.is_active() {
            break;
        }
        f.state().tick_window_animations(TICK);
        let now = chrome_alpha(&mut f, &window);
        assert!(
            now <= previous + 1e-6,
            "the chrome only ever leaves across the grow: it stepped back up from \
             {previous} to {now}"
        );
        previous = now;
    }
    assert!(
        f.state().chrome_fullscreen(&window),
        "and is gone once the window fills the output"
    );

    f.state().exit_fullscreen_on(&output);
}

/// An open fade is one arrival, not a permanent property of the entry carrying
/// it: it clears the moment it lands. The resize crossfade is suppressed while
/// it runs, so an entry that kept it forever would deny that to every later leg
/// on the same window.
#[test]
fn an_inherited_open_fade_clears_when_it_lands() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    map_window(&mut f, id, "a", (400, 300));
    let window = window_by_app_id(&mut f, "a").unwrap();
    reset_view(&mut f);
    let eid = element_id(&mut f, &window);
    let from = f.state().stage.position_of(&window).unwrap();

    // Part-way through the open fade, before it has landed.
    for _ in 0..3 {
        f.state().tick_window_animations(TICK);
    }

    // A plain move replaces the open entry with a geometry chase. The chase
    // starts its leg from zero while the fade keeps the progress it had, so the
    // two land on different ticks — which is what leaves the entry alive to be
    // asked about after the fade is done.
    f.state()
        .map_window(window.clone(), from + Point::from((300, 0)), false);
    f.state().animate_window_move_from(&window, from, None);
    assert!(
        f.state().window_animations.has_open_fade(eid),
        "the chase took the open fade over instead of destroying it"
    );
    let loc = f.state().stage.position_of(&window).unwrap().to_f64();
    let size = window.geometry().size.to_f64();
    assert!(
        f.state().animated_visual(eid, loc, size).alpha < 1.0,
        "the window is still arriving, not popped to full opacity by the move"
    );

    tick_until_fade_lands(&mut f, eid);
    assert!(
        f.state()
            .window_animations
            .geometry_visual_rect(eid)
            .is_some(),
        "precondition: the chase is still running, so there is a later leg to \
         hand anything back to"
    );
    assert!(
        !f.state().window_animations.has_open_fade(eid),
        "the fade cleared once it landed"
    );
    assert_eq!(
        f.state().animated_visual(eid, loc, size).alpha,
        1.0,
        "and clearing it changed nothing on screen — it was already opaque"
    );
}

/// Tick until `id`'s open fade clears, stopping early if the entry prunes first
/// (so a fade that never clears fails on the assertion, not on a 600-tick spin).
fn tick_until_fade_lands(f: &mut Fixture, id: ElementId) {
    for _ in 0..MAX_TICKS {
        let anims = &f.state().window_animations;
        if !anims.has_open_fade(id) || anims.geometry_visual_rect(id).is_none() {
            return;
        }
        f.state().tick_window_animations(TICK);
    }
}

/// Once the fade has landed, the next leg on the same entry ramps the chrome
/// from the picture that leg stamped: a fullscreen exit brings the border,
/// shadow and bar in from the bare fullscreen picture instead of popping them on
/// at full opacity over a still-fullscreen-sized window.
#[test]
fn a_landed_open_fade_hands_the_chrome_ramp_back_to_the_next_leg() {
    let mut f = Fixture::new();
    let output = f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let surface = map_window(&mut f, id, "a", (400, 300));
    let window = window_by_app_id(&mut f, "a").unwrap();
    reset_view(&mut f);
    let eid = element_id(&mut f, &window);

    // Fullscreen part-way through the open fade, so the fade rides onto the
    // fullscreen chase and lands well before that chase does.
    for _ in 0..3 {
        f.state().tick_window_animations(TICK);
    }
    f.client(id).window(&surface).set_fullscreen(None);
    f.double_roundtrip(id);
    super::adopt_last_configure(&mut f, id, &surface);
    tick_until_fade_lands(&mut f, eid);
    assert!(
        f.state()
            .window_animations
            .geometry_visual_rect(eid)
            .is_some()
            && !f.state().window_animations.has_open_fade(eid),
        "precondition: the fade landed while its chase is still running"
    );

    f.state().exit_fullscreen_on(&output);
    assert_eq!(
        f.state().chrome_alpha_of(Some(eid), &window),
        0.0,
        "the exit leg starts on the bare fullscreen picture and ramps from there"
    );
}

/// A picture drawn translucent cannot claim to cover its output. Exiting
/// fullscreen while the open fade is still running restates the entry's frozen
/// picture; stamping a fullscreen cover there would cull the whole scene behind
/// a window the user can see through.
#[test]
fn a_fullscreen_exit_mid_open_fade_stamps_no_cover() {
    let mut f = Fixture::new();
    let output = f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let surface = map_straight_into_fullscreen(&mut f, id, "fs", (400, 300));
    let window = window_by_app_id(&mut f, "fs").unwrap();
    let eid = element_id(&mut f, &window);

    super::adopt_last_configure(&mut f, id, &surface);
    f.state().tick_window_animations(TICK);
    let loc = f.state().stage.position_of(&window).unwrap().to_f64();
    let size = window.geometry().size.to_f64();
    assert!(
        f.state().animated_visual(eid, loc, size).alpha < 1.0,
        "precondition: mid-fade, so the window is still see-through"
    );

    f.state().exit_fullscreen_on(&output);
    assert!(
        !f.state().is_output_visually_fullscreen(&output),
        "the scene behind stays visible under a translucent picture"
    );
}

/// A translucent fullscreen picture covers nothing — but it is still a
/// fullscreen picture, and it still wears no chrome. Exiting while the fade
/// runs suppresses the cover; reading that suppression as "the frozen picture
/// was a windowed one" pops a full-opacity bar, border and shadow onto it for
/// the length of the exit's own freeze.
#[test]
fn a_fullscreen_exit_under_a_running_fade_keeps_the_bare_picture_bare() {
    let mut f = Fixture::new();
    let output = f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let surface = map_window(&mut f, id, "fs", (400, 300));
    let window = window_by_app_id(&mut f, "fs").unwrap();
    reset_view(&mut f);
    let eid = element_id(&mut f, &window);

    // Fullscreen part-way through the open fade, so the chase inherits a fade
    // the user has already seen — one the chrome ramp is not suppressed for.
    for _ in 0..2 {
        f.state().tick_window_animations(TICK);
    }
    f.client(id).window(&surface).set_fullscreen(None);
    f.double_roundtrip(id);
    super::adopt_last_configure(&mut f, id, &surface);
    assert!(
        f.state().window_animations.has_open_fade(eid),
        "precondition: the fade is still running when the exit arms"
    );

    f.state().exit_fullscreen_on(&output);
    assert!(
        !f.state().is_output_visually_fullscreen(&output),
        "precondition: a see-through picture claims no cover"
    );
    assert_eq!(
        chrome_alpha(&mut f, &window),
        0.0,
        "the frozen picture is the fullscreen one, so the shrink brings the \
         chrome in rather than starting with it"
    );
}

/// The open fade's shrink is re-applied every time the entry is drawn, so an
/// exit has to seed its shrink from the chase rect and not from what is on
/// screen — seeding from the drawn rect applies the shrink to itself and the
/// window jumps smaller on the frame the exit arms, for the length of the exit's
/// own freeze.
#[test]
fn a_fullscreen_exit_mid_open_fade_seeds_from_the_undrawn_rect() {
    let mut f = Fixture::new();
    let output = f.add_output(1, (1920, 1080));
    reset_view(&mut f);
    let id = f.add_client();
    let _surface = map_straight_into_fullscreen(&mut f, id, "fs", (400, 300));
    let window = window_by_app_id(&mut f, "fs").unwrap();
    let eid = element_id(&mut f, &window);

    // A geometry entry draws its own rect, so the target passed here is unused —
    // the same call before and after is a fair comparison of what is on screen.
    let probe = |f: &mut Fixture| {
        f.state()
            .animated_visual(eid, Point::from((0.0, 0.0)), Size::from((0.0, 0.0)))
            .size
    };
    let before = probe(&mut f);
    assert!(
        before.w < 1920.0,
        "precondition: the fade is drawing the window smaller than its rect \
         ({before:?})"
    );

    f.state().exit_fullscreen_on(&output);
    let after = probe(&mut f);
    assert!(
        (after.w - before.w).abs() < 1.0 && (after.h - before.h).abs() < 1.0,
        "the picture is continuous across the exit: {before:?} -> {after:?}"
    );
}

/// A fit's camera pan is parked behind the freeze rather than firing at the
/// action; this applies to the open-into-fit shortcut too, exactly as it does
/// for a fit fired on an already-open window.
#[test]
fn an_open_into_fit_still_parks_its_camera_pan_until_the_ack() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let surface = map_straight_into_fit(&mut f, id, "a", (400, 300));
    let window = window_by_app_id(&mut f, "a").unwrap();
    let eid = element_id(&mut f, &window);

    assert!(
        f.state().window_animations.start_held(eid),
        "the fit's resize froze the window"
    );
    assert!(
        f.state().camera_target().is_none(),
        "the pan is parked behind the freeze"
    );

    super::adopt_last_configure(&mut f, id, &surface);
    f.state().tick_window_animations(TICK);
    assert!(
        f.state().camera_target().is_some(),
        "the ack releases the parked pan"
    );
}

/// The fade never seeds at the placement rect: the very first frame drawn for
/// an open-into-fullscreen is already the fullscreen rect.
#[test]
fn an_open_into_fullscreen_fade_seeds_at_the_fullscreen_rect_not_the_placement_rect() {
    let mut f = Fixture::new();
    let output = f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let _surface = map_straight_into_fullscreen(&mut f, id, "fs", (400, 300));
    let window = window_by_app_id(&mut f, "fs").unwrap();
    let eid = element_id(&mut f, &window);

    let camera = f.state().camera();
    let viewport = crate::state::output_logical_size(&output).to_f64();
    let seeded = f
        .state()
        .window_animations
        .geometry_visual_rect(eid)
        .unwrap();
    assert_eq!(
        (seeded.loc, seeded.size),
        (camera, viewport),
        "the very first frame is already the fullscreen rect"
    );

    f.state().exit_fullscreen_on(&output);
}

/// Toggling fullscreen off before the client acks an open-into-fullscreen must
/// not pop the window to full opacity: the exit's retarget carries the open
/// fade over rather than clearing it, so a window that was never drawn stays
/// that way through the reversal too.
#[test]
fn toggling_fullscreen_off_before_the_ack_does_not_pop_an_open_fade_to_full_opacity() {
    let mut f = Fixture::new();
    let output = f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let _surface = map_straight_into_fullscreen(&mut f, id, "fs", (400, 300));
    let window = window_by_app_id(&mut f, "fs").unwrap();
    let eid = element_id(&mut f, &window);
    assert!(
        f.state().window_animations.start_held(eid),
        "frozen, unacked"
    );

    f.state().exit_fullscreen_on(&output);

    assert!(
        f.state().window_animations.has_open_fade(eid),
        "the open fade survives the exit's retarget"
    );
    let visual = f
        .state()
        .animated_visual(eid, Point::from((0.0, 0.0)), Size::from((0.0, 0.0)));
    assert!(
        visual.alpha < 1.0,
        "a window never drawn does not pop to full opacity mid-exit, got alpha \
         {}",
        visual.alpha
    );
}

/// A window pinned to screen by rule at map time is unaffected by any of this:
/// one ordinary open entry, no start freeze, at the pinned rect.
#[test]
fn a_window_pinned_to_screen_by_rule_opens_normally_with_no_hold() {
    let mut f = Fixture::with_config(
        Config::from_toml("[[window_rules]]\napp_id = \"pin\"\npinned_to_screen = true\n").unwrap(),
    );
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    map_window(&mut f, id, "pin", (320, 240));
    let window = window_by_app_id(&mut f, "pin").unwrap();
    let eid = element_id(&mut f, &window);

    assert_eq!(f.state().window_animations.len(), 1, "exactly one entry");
    assert!(
        f.state()
            .window_animations
            .geometry_visual_rect(eid)
            .is_none(),
        "an ordinary open entry, not a geometry chase"
    );
    assert!(
        !f.state().window_animations.start_held(eid),
        "no start freeze"
    );

    tick_until_settled(&mut f);
}
