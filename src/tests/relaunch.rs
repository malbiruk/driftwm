//! Relaunch + matching conformance (§9): minting the activation token, the two
//! match signals (token stash pre-/post-first-commit, identity FIFO fallback),
//! the compound adoption (z-slot + `ElementId` continuity, body geometry), the
//! pending lifecycle (relaunch-while-pending no-op, dismiss-in-flight cancel,
//! deadline GC), the "launching…" label, and token cleanup on every exit.
//!
//! The relaunched app is never really forked (a `#[cfg(test)]` seam records the
//! spawn instead); each scenario drives the "returning" client by hand and
//! presents the compositor-minted token via `xdg_activation.activate`.

use std::time::{Duration, Instant};

use driftwm::config::Config;
use driftwm::desktop_entry::DesktopEntryCache;
use smithay::utils::{Point, Size};
use wayland_client::protocol::wl_surface::WlSurface as ClientSurface;

use driftwm::window_ext::WindowExt;

use crate::state::{StageWindow, SuspendedId};

use super::client::ClientId;
use super::real::TempDir;
use super::{Fixture, map_window, server_surface, window_by_app_id};

/// The live client window with `app_id`, if any. Unlike `window_by_app_id`, it
/// skips a same-named suspended stand-in instead of stopping at it.
fn mapped_client(f: &mut Fixture, app_id: &str) -> Option<smithay::desktop::Window> {
    f.state()
        .stage
        .windows()
        .filter_map(|w| w.client())
        .find(|w| w.app_id_or_class().as_deref() == Some(app_id))
        .cloned()
}

fn origin_view(f: &mut Fixture) {
    f.state().with_output_state(|os| {
        os.zoom = 1.0;
        os.camera = Point::from((0.0, 0.0));
    });
}

/// Seat a desktop-entry cache with a launchable `{stem}.desktop` per stem.
fn inject_cache(f: &mut Fixture, tmp: &TempDir, stems: &[&str]) {
    for stem in stems {
        let contents = format!("[Desktop Entry]\nType=Application\nName={stem}\nExec={stem}\n");
        std::fs::write(tmp.path().join(format!("{stem}.desktop")), contents).unwrap();
    }
    f.state().desktop_entry_cache = Some(DesktopEntryCache::new(vec![tmp.path().to_path_buf()]));
}

/// Insert a dormant suspended stand-in whose identity resolves to `app_id`.
fn insert_suspended(
    f: &mut Fixture,
    id: u64,
    app_id: &str,
    pos: (i32, i32),
    size: (i32, i32),
) -> SuspendedId {
    f.state()
        .insert_suspended_for_test(id, Point::from(pos), Size::from(size), app_id, app_id)
}

/// First half of a client toplevel's map: create + set app_id + commit (no
/// buffer). The window is in `pending_center` at zero size.
fn begin_window(f: &mut Fixture, cid: ClientId, app_id: &str) -> ClientSurface {
    let window = f.client(cid).create_window();
    let surface = window.surface.clone();
    window.set_app_id(app_id);
    window.commit();
    f.roundtrip(cid);
    surface
}

/// Second half: attach a buffer at `size`, ack, commit, settle. This is the
/// first *sized* commit — placement (or adoption) runs here.
fn finish_window(f: &mut Fixture, cid: ClientId, surface: &ClientSurface, size: (u16, u16)) {
    let window = f.client(cid).window(surface);
    window.set_size(size.0, size.1);
    window.attach_new_buffer();
    window.ack_last_and_commit();
    f.double_roundtrip(cid);
}

/// Present `token` as `surface`'s activation token and drive the request.
fn present_token(f: &mut Fixture, cid: ClientId, surface: &ClientSurface, token: String) {
    f.client(cid).state.activation_token = Some(token);
    f.client(cid).activate(surface);
    f.roundtrip(cid);
}

/// Ack a pending resize (adoption's body-size configure) and commit it, so the
/// adopted window's geometry reflects the body size.
fn settle_resize(f: &mut Fixture, cid: ClientId, surface: &ClientSurface, size: (u16, u16)) {
    let window = f.client(cid).window(surface);
    window.set_size(size.0, size.1);
    window.attach_new_buffer();
    window.ack_last_and_commit();
    f.double_roundtrip(cid);
}

fn client_close(f: &mut Fixture, cid: ClientId, surface: &ClientSurface) {
    f.client(cid).window(surface).destroy();
    f.roundtrip(cid);
    f.dispatch();
}

/// The lone suspended stand-in, if any.
fn suspended_present(f: &mut Fixture) -> bool {
    f.state().stage.windows().any(|w| w.suspended().is_some())
}

fn token_count(f: &mut Fixture) -> usize {
    f.state().xdg_activation_state.tokens().count()
}

/// Token path, bound before first commit: the marker is honored ahead of both
/// the serial gate (our token is serial-less) and the zero-size early return
/// (the surface has no buffer yet), stashing for the placement arm. Adoption
/// preserves the suspended window's z-slot, `ElementId`, and canvas position,
/// and configures the body size.
#[test]
fn token_adopt_pre_first_commit_preserves_slot_id_and_geometry() {
    let tmp = TempDir::new();
    let mut f = Fixture::with_config(Config::default());
    f.add_output(1, (1920, 1080));
    inject_cache(&mut f, &tmp, &["myapp"]);
    origin_view(&mut f);

    let sid = insert_suspended(&mut f, 1, "myapp", (500, 500), (600, 400));
    // A second window on top so the suspended sits at z-slot 0 (not topmost).
    let bg = f.add_client();
    let bg_surface = map_window(&mut f, bg, "other", (200, 200));

    let susp = StageWindow::Suspended(f.state().find_suspended(sid).unwrap());
    let eid = f.state().stage.id_of(&susp).unwrap();
    let idx = f.state().stage.windows().position(|w| *w == susp).unwrap();

    f.state().relaunch_suspended(sid);
    assert!(
        f.state().is_suspended_launching(sid),
        "label flipped to launching"
    );
    let token = f.state().pending_relaunch_token_for_test(sid).unwrap();

    // The relaunched app maps and presents the token before its first buffer.
    let cid = f.add_client();
    let surface = begin_window(&mut f, cid, "myapp");
    present_token(&mut f, cid, &surface, token);
    // Marker honored despite zero size: the surface is stashed for adoption.
    assert_eq!(
        f.state().debug_counters()["pending_adoptions"],
        1,
        "the zero-size early return did not eat the marker token"
    );

    // First sized commit adopts.
    finish_window(&mut f, cid, &surface, (300, 200));

    let adopted = window_by_app_id(&mut f, "myapp").expect("relaunched window adopted the slot");
    assert_eq!(
        f.state().stage.id_of(&adopted),
        Some(eid),
        "ElementId preserved"
    );
    assert_eq!(
        f.state().stage.windows().position(|w| *w == adopted),
        Some(idx),
        "z-slot preserved"
    );
    assert_eq!(
        f.state().stage.position_of(&adopted),
        Some(Point::from((500, 500))),
        "seated at the suspended position"
    );
    assert!(
        f.client(cid)
            .window(&surface)
            .configures_received
            .iter()
            .any(|(_, c)| c.size == (600, 400)),
        "configured to the body size"
    );

    // The suspended stand-in and its pending relaunch are gone; token cleaned up.
    assert!(!suspended_present(&mut f), "the stand-in was replaced");
    assert_eq!(f.state().debug_counters()["pending_relaunches"], 0);
    assert_eq!(
        token_count(&mut f),
        0,
        "the token was deregistered on adopt"
    );

    // Complete the resize handshake: geometry fills the body rect.
    settle_resize(&mut f, cid, &surface, (600, 400));
    assert_eq!(
        window_by_app_id(&mut f, "myapp").unwrap().geometry().size,
        Size::from((600, 400))
    );

    client_close(&mut f, cid, &surface);
    client_close(&mut f, bg, &bg_surface);
}

/// Adopting a relaunched window into a CSD-origin stand-in reassembles the full
/// window: the stand-in shrank its body under the bar at conversion, so adopt
/// hands the app back the body height + bar, positioned a bar above the body.
/// An SSD-origin adopt (the tests above) keeps the body rect verbatim.
#[test]
fn token_adopt_of_csd_stand_in_reassembles_full_geometry() {
    let tmp = TempDir::new();
    let mut f = Fixture::with_config(Config::default());
    f.add_output(1, (1920, 1080));
    inject_cache(&mut f, &tmp, &["myapp"]);
    origin_view(&mut f);

    let bar = f.state().config.decorations.title_bar_height;
    // A CSD-origin stand-in: body (600,400) at (500,500).
    let sid = f.state().insert_suspended_csd_for_test(
        1,
        Point::from((500, 500)),
        Size::from((600, 400)),
        "myapp",
        "myapp",
    );

    f.state().relaunch_suspended(sid);
    let token = f.state().pending_relaunch_token_for_test(sid).unwrap();
    let cid = f.add_client();
    let surface = begin_window(&mut f, cid, "myapp");
    present_token(&mut f, cid, &surface, token);
    finish_window(&mut f, cid, &surface, (300, 200));

    let adopted = window_by_app_id(&mut f, "myapp").expect("relaunched window adopted the slot");
    // Positioned a bar above the body; sized to the full window (body + bar).
    assert_eq!(
        f.state().stage.position_of(&adopted),
        Some(Point::from((500, 500 - bar))),
        "adopt seats the CSD window a bar above the stand-in body"
    );
    assert!(
        f.client(cid)
            .window(&surface)
            .configures_received
            .iter()
            .any(|(_, c)| c.size == (600, 400 + bar)),
        "configured to the reassembled full size (body + bar)"
    );

    client_close(&mut f, cid, &surface);
}

/// Adopt reassembles a CSD-origin stand-in using the bar height AT ADOPT TIME,
/// not the one in effect when it suspended: a config change while the app sits
/// dormant (e.g. a hot-reload) is not replayed from conversion — the outer
/// rect drifts by the difference, the same drift a live SSD window sees across
/// a reload.
#[test]
fn token_adopt_of_csd_stand_in_uses_bar_height_at_adopt_time() {
    let tmp = TempDir::new();
    let mut f = Fixture::with_config(Config::default());
    f.add_output(1, (1920, 1080));
    inject_cache(&mut f, &tmp, &["myapp"]);
    origin_view(&mut f);

    // A CSD-origin stand-in: body (600,400) at (500,500).
    let sid = f.state().insert_suspended_csd_for_test(
        1,
        Point::from((500, 500)),
        Size::from((600, 400)),
        "myapp",
        "myapp",
    );

    // The bar height changes while the app is dormant.
    f.state().config.decorations.title_bar_height = 40;

    f.state().relaunch_suspended(sid);
    let token = f.state().pending_relaunch_token_for_test(sid).unwrap();
    let cid = f.add_client();
    let surface = begin_window(&mut f, cid, "myapp");
    present_token(&mut f, cid, &surface, token);
    finish_window(&mut f, cid, &surface, (300, 200));

    let adopted = window_by_app_id(&mut f, "myapp").expect("relaunched window adopted the slot");
    // Positioned/sized using the CURRENT bar (40), not the default (25).
    assert_eq!(
        f.state().stage.position_of(&adopted),
        Some(Point::from((500, 500 - 40))),
        "adopt used the bar height at adopt time"
    );
    assert!(
        f.client(cid)
            .window(&surface)
            .configures_received
            .iter()
            .any(|(_, c)| c.size == (600, 400 + 40)),
        "configured to body + the current bar height, not the default"
    );

    client_close(&mut f, cid, &surface);
}

/// Adopting a relaunched window back into a clustered stand-in keeps the
/// cluster: the adopted window seats at the stand-in's slot/rect, so it stays
/// snap-adjacent to the neighbor, and it inherits the stand-in's snap rect as a
/// stable rect right away (before it settles to the body size) so a close in
/// that window can't dissolve the cluster.
#[test]
#[allow(clippy::mutable_key_type)]
fn adopt_keeps_cluster_and_seeds_stable_rect() {
    use smithay::reexports::wayland_server::Resource;

    let tmp = TempDir::new();
    let mut f = Fixture::with_config(
        Config::from_toml("[decorations]\ndefault_mode = \"server\"\n").unwrap(),
    );
    f.add_output(1, (1920, 1080));
    inject_cache(&mut f, &tmp, &["myapp"]);
    origin_view(&mut f);

    // Stand-in "myapp" at a known rect; capture the rect it presents to snap.
    let sid = insert_suspended(&mut f, 1, "myapp", (500, 500), (600, 400));
    let susp = StageWindow::Suspended(f.state().find_suspended(sid).unwrap());
    let standin_rect = f.state().snap_rect_for(&susp).unwrap();
    let gap = f.state().config.snap_gap as i32;

    // A neighbor client gap-adjacent to the stand-in's right edge, y-overlapping.
    let nb = f.add_client();
    map_window(&mut f, nb, "nb", (400, 400));
    let neighbor = window_by_app_id(&mut f, "nb").unwrap();
    f.state().map_window(
        StageWindow::Client(neighbor.clone()),
        Point::from((standin_rect.x_high as i32 + gap, 500)),
        true,
    );
    let nb_elem = StageWindow::Client(neighbor.clone());
    let rects = f.state().all_windows_with_snap_rects();
    let before = driftwm::layout::cluster::cluster_of(&nb_elem, &rects, f.state().config.snap_gap);
    assert!(
        before.contains(&susp),
        "neighbor clustered with the stand-in"
    );

    // Relaunch and adopt (token presented before the first buffer).
    f.state().relaunch_suspended(sid);
    let token = f.state().pending_relaunch_token_for_test(sid).unwrap();
    let cid = f.add_client();
    let surface = begin_window(&mut f, cid, "myapp");
    present_token(&mut f, cid, &surface, token);
    // First sized commit adopts at a not-yet-body size.
    finish_window(&mut f, cid, &surface, (300, 200));
    let adopted = mapped_client(&mut f, "myapp").expect("adopted");

    // The seed: the adopted window carries the stand-in's rect immediately,
    // even though its live geometry is still the pre-body configure size.
    let seeded = f
        .state()
        .stable_snap_rects
        .get(&server_surface(&adopted).id())
        .copied()
        .expect("adopted window seeded a stable snap rect");
    assert_eq!(seeded.x_low, standin_rect.x_low);
    assert_eq!(seeded.x_high, standin_rect.x_high);
    assert_eq!(seeded.y_low, standin_rect.y_low);
    assert_eq!(seeded.y_high, standin_rect.y_high);

    // Once the client settles to the body size, the live cluster is intact with
    // the adopted window in the stand-in's place.
    settle_resize(&mut f, cid, &surface, (600, 400));
    let rects = f.state().all_windows_with_snap_rects();
    let after = driftwm::layout::cluster::cluster_of(&nb_elem, &rects, f.state().config.snap_gap);
    assert!(
        after.contains(&StageWindow::Client(adopted)),
        "the adopted live window stayed in the cluster"
    );

    client_close(&mut f, cid, &surface);
}

/// Token path, bound after the window is already mapped: adoption happens in the
/// activation handler with a fresh resize configure, and the adopted window ends
/// up focused (the suspended window held the focus intent).
#[test]
fn token_adopt_post_first_commit_focuses_adopted_window() {
    let tmp = TempDir::new();
    let mut f = Fixture::with_config(Config::default());
    f.add_output(1, (1920, 1080));
    inject_cache(&mut f, &tmp, &["myapp"]);
    origin_view(&mut f);

    let sid = insert_suspended(&mut f, 1, "myapp", (700, 300), (500, 350));
    let susp = StageWindow::Suspended(f.state().find_suspended(sid).unwrap());
    let eid = f.state().stage.id_of(&susp).unwrap();

    // The user focused the stand-in, then relaunched it.
    f.state().focus_and_raise_suspended(sid);
    assert_eq!(f.state().gated_suspended_focus(), Some(sid));
    f.state().relaunch_suspended(sid);
    let token = f.state().pending_relaunch_token_for_test(sid).unwrap();
    // Close the identity-fallback window so the window maps normally and only
    // the (post-map) token path can adopt it.
    f.state().expire_relaunch_fallback_for_test(sid);

    // The relaunched window maps fully (placed normally) before the token lands.
    let cid = f.add_client();
    let surface = map_window(&mut f, cid, "myapp", (300, 200));
    present_token(&mut f, cid, &surface, token);

    let adopted = window_by_app_id(&mut f, "myapp").unwrap();
    assert_eq!(
        f.state().stage.id_of(&adopted),
        Some(eid),
        "ElementId preserved"
    );
    assert_eq!(
        f.state().stage.position_of(&adopted),
        Some(Point::from((700, 300))),
        "relocated onto the suspended rect"
    );
    // Focus intent moved onto the adopted window.
    let server = server_surface(&adopted);
    assert_eq!(
        super::keyboard_focus(&mut f).as_ref(),
        Some(&server),
        "adopted window focused"
    );
    assert!(!suspended_present(&mut f));
    assert_eq!(token_count(&mut f), 0);

    settle_resize(&mut f, cid, &surface, (500, 350));
    client_close(&mut f, cid, &surface);
}

/// A single-instance app forwards the startup id to its already-open window,
/// which then presents our token. Token possession is proof the window is the
/// app's own answer to this relaunch, so it is adopted into the stand-in's slot:
/// relocated onto the stand-in rect, inheriting its `ElementId`, sized to the
/// body, and consuming the stand-in.
#[test]
fn already_open_same_app_window_is_adopted() {
    let tmp = TempDir::new();
    let mut f = Fixture::with_config(Config::default());
    f.add_output(1, (1920, 1080));
    inject_cache(&mut f, &tmp, &["myapp"]);
    origin_view(&mut f);

    // An existing window of the app is already open (mapped before the relaunch).
    let cid = f.add_client();
    let existing = map_window(&mut f, cid, "myapp", (300, 200));

    // A suspended stand-in of the same app is relaunched.
    let sid = insert_suspended(&mut f, 1, "myapp", (800, 500), (400, 300));
    let susp = StageWindow::Suspended(f.state().find_suspended(sid).unwrap());
    let eid = f.state().stage.id_of(&susp).unwrap();
    f.state().relaunch_suspended(sid);
    // Past the fallback window, so identity matching can't fire either — only
    // the token path can adopt the already-open window.
    f.state().expire_relaunch_fallback_for_test(sid);
    let token = f.state().pending_relaunch_token_for_test(sid).unwrap();

    // The running instance activates its EXISTING window with our token.
    present_token(&mut f, cid, &existing, token);

    // The already-open window now occupies the stand-in's stage entry: same
    // ElementId (its own prior entry was consumed by the adopt).
    let adopted = window_by_app_id(&mut f, "myapp").expect("the existing window adopted the slot");
    assert_eq!(
        f.state().stage.id_of(&adopted),
        Some(eid),
        "took the stand-in's ElementId"
    );
    assert_eq!(
        f.state().stage.position_of(&adopted),
        Some(Point::from((800, 500))),
        "relocated onto the stand-in rect"
    );
    assert!(
        f.client(cid)
            .window(&existing)
            .configures_received
            .iter()
            .any(|(_, c)| c.size == (400, 300)),
        "configured to the stand-in body size"
    );
    assert!(!suspended_present(&mut f), "the stand-in was consumed");
    assert_eq!(
        f.state().debug_counters()["pending_relaunches"],
        0,
        "the pending relaunch was consumed"
    );
    assert_eq!(
        token_count(&mut f),
        0,
        "the token was deregistered on adopt"
    );

    settle_resize(&mut f, cid, &existing, (400, 300));
    client_close(&mut f, cid, &existing);
}

/// Keyboard focus rides an already-open adopt the same as a freshly-mapped one:
/// the stand-in held focus, so the window it hands off to inherits it and gets
/// an Activated configure on the wire.
#[test]
fn already_open_adopt_focuses_and_activates_the_window() {
    let tmp = TempDir::new();
    let mut f = Fixture::with_config(Config::default());
    f.add_output(1, (1920, 1080));
    inject_cache(&mut f, &tmp, &["myapp"]);
    origin_view(&mut f);

    let cid = f.add_client();
    let existing = map_window(&mut f, cid, "myapp", (300, 200));

    let sid = insert_suspended(&mut f, 1, "myapp", (800, 500), (400, 300));
    // The stand-in holds focus — the user is waiting on this relaunch.
    f.state().focus_and_raise_suspended(sid);
    f.state().relaunch_suspended(sid);
    f.state().expire_relaunch_fallback_for_test(sid);
    let token = f.state().pending_relaunch_token_for_test(sid).unwrap();

    present_token(&mut f, cid, &existing, token);

    let adopted = window_by_app_id(&mut f, "myapp").expect("the existing window adopted the slot");
    assert_eq!(
        f.state().focused_window().as_ref(),
        Some(&adopted),
        "the adopted window inherits keyboard focus"
    );
    let configs = f.client(cid).window(&existing).format_recent_configures();
    assert!(
        configs.contains("Activated"),
        "an adopted window inheriting focus must get an Activated configure, got:\n{configs}"
    );

    settle_resize(&mut f, cid, &existing, (400, 300));
    client_close(&mut f, cid, &existing);
}

/// Two stand-ins of the same app are both relaunched; the first spawn's window
/// maps and adopts stand-in #1. The second pending relaunch's token then lands
/// on that now-placed window — last press wins: the window rehomes into
/// stand-in #2's slot instead of the already-placed token being ignored.
#[test]
fn later_token_rehomes_an_already_adopted_window() {
    let tmp = TempDir::new();
    let mut f = Fixture::with_config(Config::default());
    f.add_output(1, (1920, 1080));
    inject_cache(&mut f, &tmp, &["myapp"]);
    origin_view(&mut f);

    let sid1 = insert_suspended(&mut f, 1, "myapp", (100, 100), (400, 300));
    let sid2 = insert_suspended(&mut f, 2, "myapp", (900, 600), (500, 350));
    let susp2 = StageWindow::Suspended(f.state().find_suspended(sid2).unwrap());
    let eid2 = f.state().stage.id_of(&susp2).unwrap();

    f.state().relaunch_suspended(sid1);
    f.state().relaunch_suspended(sid2);
    let token1 = f.state().pending_relaunch_token_for_test(sid1).unwrap();
    let token2 = f.state().pending_relaunch_token_for_test(sid2).unwrap();

    // The relaunched app's window presents stand-in #1's token before its first
    // buffer and adopts that slot.
    let cid = f.add_client();
    let surface = begin_window(&mut f, cid, "myapp");
    present_token(&mut f, cid, &surface, token1);
    finish_window(&mut f, cid, &surface, (300, 200));
    assert!(
        !f.state().is_suspended_launching(sid1),
        "stand-in #1's relaunch settled first"
    );

    // The same window then presents stand-in #2's still-pending token.
    present_token(&mut f, cid, &surface, token2);

    let adopted = window_by_app_id(&mut f, "myapp").expect("adopted");
    assert_eq!(
        f.state().stage.id_of(&adopted),
        Some(eid2),
        "took stand-in #2's ElementId"
    );
    assert_eq!(
        f.state().stage.position_of(&adopted),
        Some(Point::from((900, 600))),
        "rehomed onto stand-in #2's rect"
    );
    assert!(
        f.client(cid)
            .window(&surface)
            .configures_received
            .iter()
            .any(|(_, c)| c.size == (500, 350)),
        "configured to stand-in #2's body size"
    );
    assert!(!suspended_present(&mut f), "both stand-ins were consumed");
    assert_eq!(f.state().debug_counters()["pending_relaunches"], 0);
    assert_eq!(token_count(&mut f), 0, "both tokens were deregistered");

    settle_resize(&mut f, cid, &surface, (500, 350));
    client_close(&mut f, cid, &surface);
}

/// Identity fallback (Signal B): a token-less window of the same app is adopted
/// within the 5s window, oldest pending first (FIFO), each landing on its own
/// suspended rect via `ElementId`.
#[test]
fn identity_fallback_adopts_fifo_within_window() {
    let tmp = TempDir::new();
    let mut f = Fixture::with_config(Config::default());
    f.add_output(1, (1920, 1080));
    inject_cache(&mut f, &tmp, &["myapp"]);
    origin_view(&mut f);

    let sid1 = insert_suspended(&mut f, 1, "myapp", (100, 100), (400, 300));
    let sid2 = insert_suspended(&mut f, 2, "myapp", (900, 600), (500, 350));
    let susp1 = StageWindow::Suspended(f.state().find_suspended(sid1).unwrap());
    let e1 = f.state().stage.id_of(&susp1).unwrap();
    let susp2 = StageWindow::Suspended(f.state().find_suspended(sid2).unwrap());
    let e2 = f.state().stage.id_of(&susp2).unwrap();

    // Relaunch both; sid1 was spawned first, so it adopts first.
    f.state().relaunch_suspended(sid1);
    f.state().relaunch_suspended(sid2);

    let cid = f.add_client();
    // First token-less window adopts the oldest pending (sid1).
    let s1 = map_window(&mut f, cid, "myapp", (300, 200));
    let w1 = f.state().stage.window_by_id(e1).unwrap().clone();
    assert!(w1.client().is_some(), "sid1's slot now holds a live window");
    assert_eq!(
        f.state().stage.position_of(&w1),
        Some(Point::from((100, 100)))
    );

    // Second token-less window adopts the next pending (sid2).
    let s2 = map_window(&mut f, cid, "myapp", (300, 200));
    let w2 = f.state().stage.window_by_id(e2).unwrap().clone();
    assert!(w2.client().is_some(), "sid2's slot now holds a live window");
    assert_eq!(
        f.state().stage.position_of(&w2),
        Some(Point::from((900, 600)))
    );

    assert!(!suspended_present(&mut f), "both stand-ins were adopted");
    assert_eq!(f.state().debug_counters()["pending_relaunches"], 0);
    assert_eq!(token_count(&mut f), 0);

    settle_resize(&mut f, cid, &s1, (400, 300));
    settle_resize(&mut f, cid, &s2, (500, 350));
    client_close(&mut f, cid, &s1);
    client_close(&mut f, cid, &s2);
}

/// Once the 5s fallback window closes, a token-less same-app window is NO longer
/// captured — it gets normal placement — while the relaunch itself stays pending
/// (only the identity fallback lapsed, not the whole relaunch).
#[test]
fn identity_fallback_expiry_yields_normal_placement() {
    let tmp = TempDir::new();
    let mut f = Fixture::with_config(Config::default());
    f.add_output(1, (1920, 1080));
    inject_cache(&mut f, &tmp, &["myapp"]);
    origin_view(&mut f);

    let sid = insert_suspended(&mut f, 1, "myapp", (200, 200), (400, 300));
    f.state().relaunch_suspended(sid);
    f.state().expire_relaunch_fallback_for_test(sid);

    let cid = f.add_client();
    let surface = map_window(&mut f, cid, "myapp", (300, 200));
    let mapped = mapped_client(&mut f, "myapp").expect("the window mapped");
    assert_ne!(
        f.state().stage.position_of(&mapped),
        Some(Point::from((200, 200))),
        "the expired fallback did not capture the window"
    );
    // A surviving stand-in proves the window was not adopted (adoption would
    // have consumed it).
    assert!(suspended_present(&mut f), "the stand-in is still dormant");
    assert!(
        f.state().is_suspended_launching(sid),
        "still pending after fallback lapse"
    );

    // Cleanup: dismiss cancels the pending (and its token).
    f.state().dismiss_suspended(sid);
    assert_eq!(token_count(&mut f), 0);
    client_close(&mut f, cid, &surface);
}

/// A relaunched window that entered fullscreen (its own request or a rule)
/// before presenting a late token must NOT be adopted: adoption would rip it out
/// of the fullscreen map and strand the camera park. The late-token arm dismisses
/// the stand-in and leaves the window fullscreen, camera restore intact.
#[test]
fn late_token_does_not_adopt_a_fullscreen_window() {
    let tmp = TempDir::new();
    let mut f = Fixture::with_config(Config::default());
    f.add_output(1, (1920, 1080));
    inject_cache(&mut f, &tmp, &["myapp"]);
    // No camera override: leaving the camera output-aligned keeps the fullscreen
    // park a no-op, so the blur-generation counter returns to baseline.

    let sid = insert_suspended(&mut f, 1, "myapp", (400, 300), (500, 350));
    f.state().focus_and_raise_suspended(sid);
    assert!(f.state().relaunch_suspended(sid));
    let token = f.state().pending_relaunch_token_for_test(sid).unwrap();
    // Expire the identity fallback so the window maps normally (not adopted at
    // first commit) — only the late token could adopt it.
    f.state().expire_relaunch_fallback_for_test(sid);

    // The relaunched window maps, then enters fullscreen (own request) before the
    // token lands.
    let cid = f.add_client();
    let surface = map_window(&mut f, cid, "myapp", (300, 200));
    f.client(cid).window(&surface).set_fullscreen(None);
    f.double_roundtrip(cid);
    let window = mapped_client(&mut f, "myapp").expect("mapped");
    assert!(
        f.state().is_window_fullscreen(&window),
        "the window entered fullscreen"
    );

    // The late token arrives: adoption is refused.
    present_token(&mut f, cid, &surface, token);

    assert!(
        f.state().is_window_fullscreen(&window),
        "the window stays fullscreen — not ripped out of the map"
    );
    assert!(
        !suspended_present(&mut f),
        "the obsolete stand-in was dismissed"
    );
    assert_eq!(
        f.state().debug_counters()["pending_relaunches"],
        0,
        "the pending relaunch was consumed"
    );
    assert_eq!(token_count(&mut f), 0, "the token was deregistered");

    // Camera restore intact: fullscreen exits cleanly (the debug_assert_eq in
    // exit_fullscreen_on would fire if the fullscreen halves had diverged).
    let out_name = f
        .state()
        .stage
        .fullscreen_output_of(&window)
        .unwrap()
        .to_string();
    let output = f.state().output_by_name(&out_name).unwrap();
    f.state().exit_fullscreen_on(&output);
    assert!(
        !f.state().stage.has_fullscreen(),
        "fullscreen exited cleanly"
    );

    client_close(&mut f, cid, &surface);
}

/// A widget (rule-placed off the normal window flow) that already sits open and
/// then presents a live relaunch token must NOT be adopted: hijacking it into
/// the stand-in's slot would rip it out of its rule placement. The token is
/// honored by dismissing the now-stale stand-in and leaving the widget exactly
/// where it is.
#[test]
fn late_token_does_not_adopt_a_widget_window() {
    let tmp = TempDir::new();
    let mut f = Fixture::with_config(
        Config::from_toml("[[window_rules]]\napp_id = \"myapp\"\nwidget = true\n").unwrap(),
    );
    f.add_output(1, (1920, 1080));
    inject_cache(&mut f, &tmp, &["myapp"]);
    origin_view(&mut f);

    // A widget of the app is already open (rule-placed before the relaunch).
    let cid = f.add_client();
    let widget = map_window(&mut f, cid, "myapp", (300, 200));
    let widget_win = window_by_app_id(&mut f, "myapp").unwrap();
    let pos_before = f.state().stage.position_of(&widget_win);

    // A suspended stand-in of the same app is relaunched.
    let sid = insert_suspended(&mut f, 1, "myapp", (800, 500), (400, 300));
    f.state().relaunch_suspended(sid);
    let token = f.state().pending_relaunch_token_for_test(sid).unwrap();

    // The widget's own client presents the token back.
    present_token(&mut f, cid, &widget, token);

    assert_eq!(
        f.state().stage.position_of(&widget_win),
        pos_before,
        "the widget was not relocated"
    );
    assert!(
        !suspended_present(&mut f),
        "the now-stale stand-in was dismissed"
    );
    assert_eq!(
        f.state().debug_counters()["pending_relaunches"],
        0,
        "the pending relaunch was consumed"
    );
    assert_eq!(token_count(&mut f), 0, "the token was deregistered");

    client_close(&mut f, cid, &widget);
}

/// A dismiss while a relaunch is in flight cancels it: the token is deregistered
/// on the spot, so a late presentation is a no-op and the window maps normally.
#[test]
fn dismiss_in_flight_lets_late_token_map_normally() {
    let tmp = TempDir::new();
    let mut f = Fixture::with_config(Config::default());
    f.add_output(1, (1920, 1080));
    inject_cache(&mut f, &tmp, &["myapp"]);
    origin_view(&mut f);

    let sid = insert_suspended(&mut f, 1, "myapp", (300, 300), (400, 300));
    f.state().relaunch_suspended(sid);
    let token = f.state().pending_relaunch_token_for_test(sid).unwrap();

    // The user dismisses the stand-in before the app comes back.
    f.state().dismiss_suspended(sid);
    assert!(!suspended_present(&mut f));
    assert_eq!(f.state().debug_counters()["pending_relaunches"], 0);
    assert_eq!(
        token_count(&mut f),
        0,
        "the token was deregistered on dismiss"
    );

    // The relaunched window presents the now-stale token and maps normally.
    let cid = f.add_client();
    let surface = begin_window(&mut f, cid, "myapp");
    present_token(&mut f, cid, &surface, token);
    assert_eq!(
        f.state().debug_counters()["pending_adoptions"],
        0,
        "a stale token leaves no stash"
    );
    finish_window(&mut f, cid, &surface, (300, 200));
    assert!(
        window_by_app_id(&mut f, "myapp").is_some(),
        "the window mapped normally"
    );

    client_close(&mut f, cid, &surface);
}

/// A second relaunch while one is pending is a no-op: no second token, no second
/// spawn.
#[test]
fn relaunch_while_pending_is_noop() {
    let tmp = TempDir::new();
    let mut f = Fixture::with_config(Config::default());
    f.add_output(1, (1920, 1080));
    inject_cache(&mut f, &tmp, &["myapp"]);
    origin_view(&mut f);

    let sid = insert_suspended(&mut f, 1, "myapp", (300, 300), (400, 300));
    // Clear any spawns from sibling scenarios sharing this thread.
    f.state().take_relaunch_spawns_for_test();

    f.state().relaunch_suspended(sid);
    let token = f.state().pending_relaunch_token_for_test(sid).unwrap();

    f.state().relaunch_suspended(sid);
    assert_eq!(
        f.state().pending_relaunch_token_for_test(sid),
        Some(token),
        "the token is unchanged (no re-mint)"
    );
    assert_eq!(f.state().debug_counters()["pending_relaunches"], 1);
    assert_eq!(
        f.state().take_relaunch_spawns_for_test().len(),
        1,
        "the app was spawned exactly once"
    );

    f.state().dismiss_suspended(sid);
}

/// The launching label flips on relaunch and reverts when the 30s deadline GCs
/// the pending relaunch, deregistering its token.
#[test]
fn launching_label_reverts_on_deadline() {
    let tmp = TempDir::new();
    let mut f = Fixture::with_config(Config::default());
    f.add_output(1, (1920, 1080));
    inject_cache(&mut f, &tmp, &["myapp"]);
    origin_view(&mut f);

    let sid = insert_suspended(&mut f, 1, "myapp", (300, 300), (400, 300));
    assert!(!f.state().is_suspended_launching(sid));

    f.state().relaunch_suspended(sid);
    assert!(f.state().is_suspended_launching(sid));
    assert_eq!(token_count(&mut f), 1);

    // The relaunch never materialized (single-instance app focused its existing
    // window); the deadline sweep reclaims it.
    f.state()
        .sweep_pending_relaunches(Instant::now() + Duration::from_secs(31));
    assert!(!f.state().is_suspended_launching(sid), "label reverted");
    assert_eq!(f.state().debug_counters()["pending_relaunches"], 0);
    assert_eq!(token_count(&mut f), 0, "the token was deregistered on GC");
    assert!(suspended_present(&mut f), "the stand-in remains dormant");

    f.state().dismiss_suspended(sid);
}

/// An app that no longer resolves to a launchable entry leaves the window
/// dormant: no token, no pending, no spawn.
#[test]
fn relaunch_of_vanished_entry_stays_dormant() {
    let tmp = TempDir::new();
    let mut f = Fixture::with_config(Config::default());
    f.add_output(1, (1920, 1080));
    // The cache has some other app, but not "myapp".
    inject_cache(&mut f, &tmp, &["something-else"]);
    origin_view(&mut f);
    f.state().take_relaunch_spawns_for_test();

    let sid = insert_suspended(&mut f, 1, "myapp", (300, 300), (400, 300));
    f.state().relaunch_suspended(sid);

    assert!(
        !f.state().is_suspended_launching(sid),
        "no pending for a vanished entry"
    );
    assert_eq!(token_count(&mut f), 0);
    assert!(
        f.state().take_relaunch_spawns_for_test().is_empty(),
        "nothing spawned"
    );
    assert!(suspended_present(&mut f));

    f.state().dismiss_suspended(sid);
}

/// `msg relaunch <id>` calls `relaunch_suspended` for the selected stand-in:
/// the label flips to launching and the app is spawned with the minted token.
#[test]
fn ipc_relaunch_triggers_relaunch_suspended() {
    use crate::ipc::protocol::{Request, Response, WindowSelector};

    let tmp = TempDir::new();
    let mut f = Fixture::with_config(Config::default());
    f.add_output(1, (1920, 1080));
    inject_cache(&mut f, &tmp, &["myapp"]);
    origin_view(&mut f);
    f.state().take_relaunch_spawns_for_test();

    let sid = insert_suspended(&mut f, 1, "myapp", (300, 300), (400, 300));
    let element = StageWindow::Suspended(f.state().find_suspended(sid).unwrap());
    let ipc_id = f.state().stage.id_of(&element).unwrap().0;

    let reply = crate::ipc::dispatch(
        Request::Relaunch(Some(WindowSelector::Id(ipc_id))),
        f.state(),
    );
    assert!(matches!(reply, Ok(Response::Ok)));
    assert!(
        f.state().is_suspended_launching(sid),
        "msg relaunch started a pending relaunch"
    );
    assert_eq!(
        f.state().take_relaunch_spawns_for_test().len(),
        1,
        "the app was spawned"
    );

    f.state().dismiss_suspended(sid);
}

/// An adopted window that inherits the stand-in's focus must receive its
/// Activated hint on the wire. Activation is no longer granted at birth, and the
/// adopt path skips normal placement, so the hint is staged to ride the adopt
/// (decoration-tail) configure rather than sitting pending forever.
#[test]
fn adopt_inheriting_focus_delivers_activated() {
    let tmp = TempDir::new();
    let mut f = Fixture::with_config(Config::default());
    f.add_output(1, (1920, 1080));
    inject_cache(&mut f, &tmp, &["myapp"]);
    origin_view(&mut f);

    let sid = insert_suspended(&mut f, 1, "myapp", (500, 500), (600, 400));
    // The stand-in holds focus — the user is waiting on this relaunch.
    f.state().focus_and_raise_suspended(sid);

    f.state().relaunch_suspended(sid);
    let token = f.state().pending_relaunch_token_for_test(sid).unwrap();

    let cid = f.add_client();
    let surface = begin_window(&mut f, cid, "myapp");
    present_token(&mut f, cid, &surface, token);
    // First sized commit adopts the slot.
    finish_window(&mut f, cid, &surface, (300, 200));

    let adopted = window_by_app_id(&mut f, "myapp").expect("relaunched window adopted the slot");
    assert_eq!(
        f.state().focused_window().as_ref(),
        Some(&adopted),
        "the adopted window inherits keyboard focus"
    );
    let configs = f.client(cid).window(&surface).format_recent_configures();
    assert!(
        configs.contains("Activated"),
        "an adopted window inheriting focus must get an Activated configure, got:\n{configs}"
    );
}

/// `msg relaunch` on a selector that names no suspended window errors instead
/// of silently doing nothing.
#[test]
fn ipc_relaunch_errors_on_unknown_selector() {
    use crate::ipc::protocol::{Request, WindowSelector};

    let mut f = Fixture::with_config(Config::default());
    f.add_output(1, (1920, 1080));

    let reply = crate::ipc::dispatch(
        Request::Relaunch(Some(WindowSelector::AppId("nope".into()))),
        f.state(),
    );
    assert!(reply.is_err());
}
