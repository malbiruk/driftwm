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

use driftwm::config::{Action, Config};
use driftwm::desktop_entry::DesktopEntryCache;
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
use smithay::utils::{Point, Size};
use wayland_client::protocol::wl_surface::WlSurface as ClientSurface;

use driftwm::window_ext::WindowExt;

use crate::state::{ClusterResizeSnapshot, StageWindow, SuspendedId};

use super::client::ClientId;
use super::real::TempDir;
use super::{
    Fixture, adopt_last_configure, client_sees_maximized, config, end_grab,
    install_client_resize_grab, is_activated, map_window, motion, server_surface, window_by_app_id,
};

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

/// The live client windows in MRU (focus-history) order, front = most recent.
fn mru_client_order(f: &mut Fixture) -> Vec<smithay::desktop::Window> {
    f.state()
        .stage
        .focus_history()
        .iter()
        .filter_map(|w| w.client().cloned())
        .collect()
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
    assert_eq!(
        f.state().stage.windows().position(|w| *w == adopted),
        Some(idx),
        "z-slot survived the settle, not just the adopt"
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
/// snap-adjacent to the neighbor. Its stable snap rect is owed until the client
/// commits the size the adopt configured — writing one earlier would describe a
/// footprint the client has not drawn — and lands on the stand-in's rect at that
/// settle, so a close in that window can't dissolve the cluster.
#[test]
#[allow(clippy::mutable_key_type)]
fn adopt_seeds_the_stable_rect_when_the_client_settles() {
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

    // Owed, not written: the live geometry is still the pre-body configure size.
    let adopted_id = server_surface(&adopted).id();
    assert!(
        f.state().pending_adopt_settle.contains_key(&adopted_id),
        "the adopt owes a stable snap rect"
    );
    assert!(
        !f.state().stable_snap_rects.contains_key(&adopted_id),
        "no stable rect asserted at a size the client has not committed"
    );

    // Once the client settles to the body size, the debt is paid at the
    // window's own footprint — the stand-in's slot (500, 500) and body 600x400,
    // borderless, and without the textless bar every stand-in carries — and the
    // live cluster is intact with the adopted window in the stand-in's place.
    settle_resize(&mut f, cid, &surface, (600, 400));
    let seeded = f
        .state()
        .stable_snap_rects
        .get(&adopted_id)
        .copied()
        .expect("the settle seeded a stable snap rect");
    assert_eq!(seeded.x_low, 500.0);
    assert_eq!(seeded.x_high, 1100.0);
    assert_eq!(seeded.y_low, 500.0);
    assert_eq!(seeded.y_high, 900.0);
    assert!(
        !f.state().pending_adopt_settle.contains_key(&adopted_id),
        "the settle consumed the owed-rect entry"
    );

    let rects = f.state().all_windows_with_snap_rects();
    let after = driftwm::layout::cluster::cluster_of(&nb_elem, &rects, f.state().config.snap_gap);
    assert!(
        after.contains(&StageWindow::Client(adopted)),
        "the adopted live window stayed in the cluster"
    );

    client_close(&mut f, cid, &surface);
}

/// A relaunched client that acks the adopt configure before it redraws keeps
/// committing its pre-adopt (larger) size for a frame or two. Once acked, the
/// unacked-configure bail goes blind, so a stable snap rect asserted at adopt
/// time would make that stale frame read as a grow past the settled footprint —
/// and `reflow_grown_snapped_window` answers a grow into a neighbor by moving
/// the window beside it, straight out of the slot it just adopted.
#[test]
fn adopt_early_ack_straggler_keeps_the_slot_beside_a_neighbor() {
    use smithay::reexports::wayland_server::Resource;

    let tmp = TempDir::new();
    let mut f = Fixture::with_config(Config::default());
    f.add_output(1, (1920, 1080));
    inject_cache(&mut f, &tmp, &["myapp"]);
    origin_view(&mut f);

    let sid = insert_suspended(&mut f, 1, "myapp", (500, 500), (400, 300));

    // A neighbor gap-adjacent to the stand-in's right edge and y-overlapping —
    // the adjacency the reflow needs before it will relocate anything.
    let susp = StageWindow::Suspended(f.state().find_suspended(sid).unwrap());
    let standin_rect = f.state().snap_rect_for(&susp).unwrap();
    let gap = f.state().config.snap_gap as i32;
    let nb = f.add_client();
    map_window(&mut f, nb, "nb", (400, 400));
    let neighbor = window_by_app_id(&mut f, "nb").unwrap();
    f.state().map_window(
        StageWindow::Client(neighbor),
        Point::from((standin_rect.x_high as i32 + gap, 500)),
        true,
    );

    // Relaunch, then adopt on a first commit that is larger than the stand-in's
    // body — the adopt answers with a body-size configure the client owes.
    f.state().relaunch_suspended(sid);
    let token = f.state().pending_relaunch_token_for_test(sid).unwrap();
    let cid = f.add_client();
    let surface = begin_window(&mut f, cid, "myapp");
    present_token(&mut f, cid, &surface, token);
    finish_window(&mut f, cid, &surface, (700, 500));

    let adopted = mapped_client(&mut f, "myapp").expect("adopted");
    assert_eq!(
        f.state().stage.position_of(&adopted),
        Some(Point::from((500, 500))),
        "precondition: the adopt seated the window in the stand-in's slot"
    );

    // Early ack: the configure is acked without a resized frame behind it, so
    // pending configures is now empty.
    f.client(cid).window(&surface).ack_last();

    // Straggler: another pre-adopt-sized frame lands after that ack.
    let window = f.client(cid).window(&surface);
    window.attach_new_buffer();
    window.commit();
    f.double_roundtrip(cid);

    assert_eq!(
        f.state().stage.position_of(&adopted),
        Some(Point::from((500, 500))),
        "a stale pre-adopt frame must not reflow the window out of the slot"
    );

    // Already acked above, so this only draws the resize — re-acking the same
    // serial would be a protocol error.
    let window = f.client(cid).window(&surface);
    window.set_size(400, 300);
    window.attach_new_buffer();
    window.commit();
    f.double_roundtrip(cid);

    assert_eq!(
        f.state().stage.position_of(&adopted),
        Some(Point::from((500, 500))),
        "the settled window keeps the slot"
    );
    let adopted_id = server_surface(&adopted).id();
    assert!(
        !f.state().pending_adopt_settle.contains_key(&adopted_id),
        "the settle consumed the owed-rect entry"
    );
    assert!(
        f.state().stable_snap_rects.contains_key(&adopted_id),
        "and paid it off with a rect the client has actually drawn"
    );

    client_close(&mut f, cid, &surface);
}

/// The same early-ack straggler with nothing gap-adjacent to the stand-in: the
/// reflow needs a neighbor to anchor re-placement, so a lone window must ride
/// out the straggler untouched.
#[test]
fn adopt_early_ack_straggler_keeps_the_slot_without_a_neighbor() {
    use smithay::reexports::wayland_server::Resource;

    let tmp = TempDir::new();
    let mut f = Fixture::with_config(Config::default());
    f.add_output(1, (1920, 1080));
    inject_cache(&mut f, &tmp, &["myapp"]);
    origin_view(&mut f);

    let sid = insert_suspended(&mut f, 1, "myapp", (500, 500), (400, 300));
    f.state().relaunch_suspended(sid);
    let token = f.state().pending_relaunch_token_for_test(sid).unwrap();
    let cid = f.add_client();
    let surface = begin_window(&mut f, cid, "myapp");
    present_token(&mut f, cid, &surface, token);
    finish_window(&mut f, cid, &surface, (700, 500));

    let adopted = mapped_client(&mut f, "myapp").expect("adopted");

    f.client(cid).window(&surface).ack_last();
    let window = f.client(cid).window(&surface);
    window.attach_new_buffer();
    window.commit();
    f.double_roundtrip(cid);

    assert_eq!(
        f.state().stage.position_of(&adopted),
        Some(Point::from((500, 500))),
        "a lone adopted window rides out the straggler in its slot"
    );

    let window = f.client(cid).window(&surface);
    window.set_size(400, 300);
    window.attach_new_buffer();
    window.commit();
    f.double_roundtrip(cid);

    assert_eq!(
        f.state().stage.position_of(&adopted),
        Some(Point::from((500, 500))),
        "and settles there"
    );
    let adopted_id = server_surface(&adopted).id();
    assert!(
        !f.state().pending_adopt_settle.contains_key(&adopted_id),
        "the settle consumed the owed-rect entry"
    );
    assert!(
        f.state().stable_snap_rects.contains_key(&adopted_id),
        "and paid it off with a rect the client has actually drawn"
    );

    client_close(&mut f, cid, &surface);
}

/// The owed rect is per-surface state on a client that may never pay it: one
/// that closes before committing the adopt size must take the entry with it.
#[test]
fn an_adopt_that_never_settles_drops_its_owed_rect_on_close() {
    use smithay::reexports::wayland_server::Resource;

    let tmp = TempDir::new();
    let mut f = Fixture::with_config(Config::default());
    f.add_output(1, (1920, 1080));
    inject_cache(&mut f, &tmp, &["myapp"]);
    origin_view(&mut f);

    let sid = insert_suspended(&mut f, 1, "myapp", (500, 500), (400, 300));
    f.state().relaunch_suspended(sid);
    let token = f.state().pending_relaunch_token_for_test(sid).unwrap();
    let cid = f.add_client();
    let surface = begin_window(&mut f, cid, "myapp");
    present_token(&mut f, cid, &surface, token);
    finish_window(&mut f, cid, &surface, (700, 500));

    let adopted = mapped_client(&mut f, "myapp").expect("adopted");
    assert!(
        f.state()
            .pending_adopt_settle
            .contains_key(&server_surface(&adopted).id()),
        "precondition: the adopt left a rect owed"
    );

    client_close(&mut f, cid, &surface);
    assert_eq!(
        f.state().debug_counters()["pending_adopt_settle"],
        0,
        "the surface teardown took the owed-rect entry with it"
    );
}

/// The reflow's own worst case — a client that maps small and jumps to its real
/// render size a frame later — landing on a window that was just adopted. The
/// jump is not the size the adopt configured, so the rect stays owed. Had the
/// adopt left the one from the window's pre-adopt placement standing, that jump
/// would read as a grow past a settled footprint sited where the window used to
/// be, and the reflow would drag it back beside its former neighbor.
#[test]
fn an_adopted_window_that_jumps_size_without_settling_keeps_the_slot() {
    use smithay::reexports::wayland_server::Resource;

    let tmp = TempDir::new();
    let mut f = Fixture::with_config(Config::default());
    f.add_output(1, (1920, 1080));
    inject_cache(&mut f, &tmp, &["myapp"]);
    origin_view(&mut f);

    // The app is already open and normally placed, so it carries a stable snap
    // rect at that site.
    let cid = f.add_client();
    let existing = map_window(&mut f, cid, "myapp", (300, 200));
    let win = window_by_app_id(&mut f, "myapp").unwrap();
    let placed = f
        .state()
        .snap_rect_for(&StageWindow::Client(win.clone()))
        .expect("the open window has a footprint");
    let gap = f.state().config.snap_gap as i32;

    // Gap-adjacent to that placement: the neighbor a reflow would re-place
    // against.
    let nb = f.add_client();
    map_window(&mut f, nb, "nb", (400, 400));
    let neighbor = window_by_app_id(&mut f, "nb").unwrap();
    f.state().map_window(
        StageWindow::Client(neighbor),
        Point::from((placed.x_high as i32 + gap, placed.y_low as i32)),
        true,
    );

    // The stand-in's slot is a canvas away, with a window just under it for the
    // jumped size to run into — the reflow only moves a grow that collides.
    let (slot_x, slot_y) = (placed.x_low as i32 - 1000, placed.y_low as i32);
    let below = f.add_client();
    map_window(&mut f, below, "below", (200, 100));
    let below_win = window_by_app_id(&mut f, "below").unwrap();
    f.state().map_window(
        StageWindow::Client(below_win),
        Point::from((slot_x + 100, slot_y + 450)),
        true,
    );

    let sid = insert_suspended(&mut f, 1, "myapp", (slot_x, slot_y), (600, 400));
    f.state().relaunch_suspended(sid);
    f.state().expire_relaunch_fallback_for_test(sid);
    let token = f.state().pending_relaunch_token_for_test(sid).unwrap();
    present_token(&mut f, cid, &existing, token);
    assert_eq!(
        f.state().stage.position_of(&win),
        Some(Point::from((slot_x, slot_y))),
        "precondition: the token adopted the open window into the stand-in's slot"
    );

    // The app draws its real size instead of the body size the adopt asked for.
    let window = f.client(cid).window(&existing);
    window.set_size(800, 600);
    window.attach_new_buffer();
    window.ack_last_and_commit();
    f.double_roundtrip(cid);

    assert_eq!(
        f.state().stage.position_of(&win),
        Some(Point::from((slot_x, slot_y))),
        "the jump must not drag the window back beside its pre-adopt neighbor"
    );
    assert!(
        f.state()
            .pending_adopt_settle
            .contains_key(&server_surface(&win).id()),
        "a size that isn't the adopt's leaves the rect owed"
    );

    client_close(&mut f, cid, &existing);
}

/// The settle pays the debt off with a rect, so it may only consume it when
/// there is one to write. A screen-pinned window lives outside the canvas and
/// has no snap rect at all: taking the entry there would leave it with no stable
/// rect from either side.
#[test]
fn a_pinned_adopt_keeps_owing_its_stable_rect() {
    use smithay::reexports::wayland_server::Resource;

    let tmp = TempDir::new();
    let mut f = Fixture::with_config(Config::default());
    f.add_output(1, (1920, 1080));
    inject_cache(&mut f, &tmp, &["myapp"]);
    origin_view(&mut f);

    // The stand-in holds focus, so the adopted window inherits it and is what
    // the pin action acts on.
    let sid = insert_suspended(&mut f, 1, "myapp", (500, 500), (400, 300));
    f.state().focus_and_raise_suspended(sid);
    f.state().relaunch_suspended(sid);
    let token = f.state().pending_relaunch_token_for_test(sid).unwrap();
    let cid = f.add_client();
    let surface = begin_window(&mut f, cid, "myapp");
    present_token(&mut f, cid, &surface, token);
    finish_window(&mut f, cid, &surface, (700, 500));

    let adopted = mapped_client(&mut f, "myapp").expect("adopted");
    f.state().execute_action(&Action::TogglePinToScreen);
    assert!(
        f.state().is_pinned(&adopted),
        "precondition: the adopted window pinned before it settled"
    );

    settle_resize(&mut f, cid, &surface, (400, 300));
    assert!(
        f.state()
            .pending_adopt_settle
            .contains_key(&server_surface(&adopted).id()),
        "the debt stands while there is no footprint to record"
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
    use smithay::reexports::wayland_server::Resource;

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

    // The window arrived fully placed, so it carried a stable snap rect at the
    // site it just left. That rect describes neither where the window is now nor
    // what it will draw, so the adopt takes it away and owes one instead.
    let adopted_id = server_surface(&adopted).id();
    assert!(
        !f.state().stable_snap_rects.contains_key(&adopted_id),
        "the pre-adopt placement's stable rect left with the move"
    );
    assert!(
        f.state().pending_adopt_settle.contains_key(&adopted_id),
        "the adopt owes one until the client draws the body size"
    );

    // The slot is bigger than the window was, so the settle commit is itself a
    // grow — paid off at the adopted footprint before anything can read it as
    // growth past the old one.
    settle_resize(&mut f, cid, &existing, (400, 300));
    let seeded = f
        .state()
        .stable_snap_rects
        .get(&adopted_id)
        .copied()
        .expect("the settle seeded a stable snap rect");
    assert_eq!(
        (seeded.x_low, seeded.y_low, seeded.x_high, seeded.y_high),
        (800.0, 500.0, 1200.0, 800.0),
        "seeded at the stand-in's slot, not the window's pre-adopt footprint"
    );

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

/// An already-fit window that forwards a relaunch token is adopted the same as
/// any other already-open window (fit is not one of the exclusions) — but the
/// adopt configure must clear the client's `Maximized`, or its restore button
/// is left permanently dead: the adopted window inherits the stand-in's
/// fit-less stage entry, so the `unmaximize_request` that button dispatches
/// finds `unfit_window` early-returning at a `None` `fit_saved_size`. Same bug
/// class as the four resize arms in `resize_parity.rs` / `gesture_resize.rs`;
/// this is the fifth arm.
#[test]
fn adopt_of_an_already_fit_window_clears_the_client_maximized_state() {
    let tmp = TempDir::new();
    let mut f = Fixture::with_config(Config::default());
    f.add_output(1, (1920, 1080));
    inject_cache(&mut f, &tmp, &["myapp"]);
    origin_view(&mut f);

    let cid = f.add_client();
    let existing = map_window(&mut f, cid, "myapp", (300, 200));
    let win = window_by_app_id(&mut f, "myapp").unwrap();

    f.state().toggle_fit_window(&win);
    f.double_roundtrip(cid);
    assert!(
        client_sees_maximized(&mut f, cid, &existing),
        "precondition: the fit told the client it is maximized"
    );

    let sid = insert_suspended(&mut f, 1, "myapp", (800, 500), (400, 300));
    f.state().relaunch_suspended(sid);
    // Past the fallback window, so identity matching can't fire — only the
    // token path adopts the already-fit window.
    f.state().expire_relaunch_fallback_for_test(sid);
    let token = f.state().pending_relaunch_token_for_test(sid).unwrap();

    present_token(&mut f, cid, &existing, token);

    let adopted = window_by_app_id(&mut f, "myapp").expect("adopted");
    assert_eq!(
        f.state().stage.position_of(&adopted),
        Some(Point::from((800, 500))),
        "precondition: adopt seated the window at the stand-in's slot"
    );
    assert!(
        !client_sees_maximized(&mut f, cid, &existing),
        "the adopt configure told the client it is no longer maximized"
    );

    settle_resize(&mut f, cid, &existing, (400, 300));
    client_close(&mut f, cid, &existing);
}

/// A window mid fit-exit settle — the client has not yet acked the restore
/// configure, so a `pending_recenter` is still owed — that then forwards a
/// live relaunch token must not have that stale recenter fire once it settles
/// into the stand-in's slot: the recenter's `target_center` is the window's
/// OLD pre-fit position, so completing it would re-map the freshly adopted
/// window right back out of the slot the adopt just seated it in.
#[test]
fn adopt_drops_an_owed_fit_exit_recenter_so_it_cannot_pull_the_window_out_of_the_slot() {
    use smithay::reexports::wayland_server::Resource;

    let tmp = TempDir::new();
    let mut f = Fixture::with_config(Config::default());
    f.add_output(1, (1920, 1080));
    inject_cache(&mut f, &tmp, &["myapp"]);
    origin_view(&mut f);

    let cid = f.add_client();
    let surface = map_window(&mut f, cid, "myapp", (300, 200));
    let win = window_by_app_id(&mut f, "myapp").unwrap();

    // Fit, then adopt the fit size as a real client would.
    f.state().toggle_fit_window(&win);
    f.double_roundtrip(cid);
    let (fw, fh) = f
        .client(cid)
        .window(&surface)
        .configures_received
        .last()
        .unwrap()
        .1
        .size;
    let cw = f.client(cid).window(&surface);
    cw.set_size(fw as u16, fh as u16);
    cw.attach_new_buffer();
    cw.ack_last_and_commit();
    f.double_roundtrip(cid);
    assert!(f.state().stage.is_fit(&win), "precondition: fit");

    // Unfit: a different-size exit, so a real pending_recenter is left owed —
    // the client never acks this restore configure.
    f.state().toggle_fit_window(&win);
    let root = server_surface(&win);
    assert!(
        f.state().pending_recenter.contains_key(&root.id()),
        "precondition: an unfit-exit recenter is owed"
    );

    // Before that recenter ever settles, a relaunch token lands on this same
    // live window (the app forwards it, single-instance style) and adopts it
    // into a stand-in's slot elsewhere on the canvas.
    let sid = insert_suspended(&mut f, 1, "myapp", (1400, 900), (400, 300));
    f.state().relaunch_suspended(sid);
    f.state().expire_relaunch_fallback_for_test(sid);
    let token = f.state().pending_relaunch_token_for_test(sid).unwrap();
    present_token(&mut f, cid, &surface, token);

    let adopted = window_by_app_id(&mut f, "myapp").expect("adopted");
    assert_eq!(
        f.state().stage.position_of(&adopted),
        Some(Point::from((1400, 900))),
        "precondition: adopt seated the window at the stand-in's slot"
    );

    // The client acks the adopt configure at the stand-in's body size — a size
    // change from the still-outstanding fit-exit's pre_exit_size, exactly the
    // commit that would fire a surviving recenter.
    settle_resize(&mut f, cid, &surface, (400, 300));

    assert_eq!(
        f.state().stage.position_of(&adopted),
        Some(Point::from((1400, 900))),
        "the adopt configure's own settle must not re-map the window out of the stand-in's slot"
    );

    client_close(&mut f, cid, &surface);
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

/// A dialog (a toplevel with a parent) that presents a live relaunch token must
/// NOT be adopted. Every suspend path excludes dialogs, so no stand-in ever
/// stands for one; adopting the dialog would tear a preferences window off its
/// parent. The token is honored by dismissing the stale stand-in and leaving the
/// dialog with its parent.
#[test]
fn late_token_does_not_adopt_a_dialog() {
    let tmp = TempDir::new();
    let mut f = Fixture::with_config(Config::default());
    f.add_output(1, (1920, 1080));
    inject_cache(&mut f, &tmp, &["myapp"]);
    origin_view(&mut f);

    // A single-instance app is already open with a child dialog (same client —
    // a toplevel's parent must be its own client's toplevel).
    let cid = f.add_client();
    let parent = map_window(&mut f, cid, "myapp", (300, 200));
    let parent_toplevel = f.client(cid).window(&parent).xdg_toplevel.clone();
    let dialog = f.client(cid).create_window();
    let dsurface = dialog.surface.clone();
    dialog.set_app_id("dialog");
    dialog.set_parent(Some(&parent_toplevel));
    dialog.commit();
    f.roundtrip(cid);
    let dwin = f.client(cid).window(&dsurface);
    dwin.set_size(300, 200);
    dwin.attach_new_buffer();
    dwin.ack_last_and_commit();
    f.double_roundtrip(cid);
    let dialog_win = window_by_app_id(&mut f, "dialog").unwrap();
    let pos_before = f.state().stage.position_of(&dialog_win);

    // A suspended stand-in of the app is relaunched; the dialog forwards the token.
    let sid = insert_suspended(&mut f, 1, "myapp", (800, 500), (400, 300));
    f.state().relaunch_suspended(sid);
    let token = f.state().pending_relaunch_token_for_test(sid).unwrap();
    present_token(&mut f, cid, &dsurface, token);

    assert_eq!(
        f.state().stage.position_of(&dialog_win),
        pos_before,
        "the dialog was not relocated"
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

    client_close(&mut f, cid, &dsurface);
    client_close(&mut f, cid, &parent);
}

/// A screen-pinned window that presents a live relaunch token must NOT be
/// adopted: hijacking it into the stand-in slot would rip it out of its pin.
/// Same carve-out branch as fullscreen — the token dismisses the stale stand-in
/// and leaves the window pinned at its site.
#[test]
fn late_token_does_not_adopt_a_pinned_window() {
    let tmp = TempDir::new();
    let mut f = Fixture::with_config(
        Config::from_toml("[[window_rules]]\napp_id = \"myapp\"\npinned_to_screen = true\n")
            .unwrap(),
    );
    f.add_output(1, (1920, 1080));
    inject_cache(&mut f, &tmp, &["myapp"]);
    origin_view(&mut f);

    // A pinned window of the app is already open (rule-pinned at map).
    let cid = f.add_client();
    let win_surface = map_window(&mut f, cid, "myapp", (300, 200));
    let pinned = window_by_app_id(&mut f, "myapp").unwrap();
    assert!(f.state().is_pinned(&pinned), "the window pinned via rule");
    let site_before = f.state().stage.pin_of(&pinned).cloned();

    // A suspended stand-in of the same app is relaunched; the pinned window
    // forwards the token back.
    let sid = insert_suspended(&mut f, 1, "myapp", (800, 500), (400, 300));
    f.state().relaunch_suspended(sid);
    let token = f.state().pending_relaunch_token_for_test(sid).unwrap();
    present_token(&mut f, cid, &win_surface, token);

    assert!(
        f.state().is_pinned(&pinned),
        "the window stayed pinned — not adopted"
    );
    assert_eq!(
        f.state().stage.pin_of(&pinned).cloned(),
        site_before,
        "the pin site is unchanged"
    );
    assert!(
        !suspended_present(&mut f),
        "the stale stand-in was dismissed"
    );
    assert_eq!(f.state().debug_counters()["pending_relaunches"], 0);
    assert_eq!(token_count(&mut f), 0, "the token was deregistered");

    client_close(&mut f, cid, &win_surface);
}

/// Adopting an UNFOCUSED already-open window preserves its MRU *slot*, not just
/// its presence. The stand-in didn't hold focus and a newer window does, so the
/// refocus path doesn't run — the adopted window must keep its exact place in
/// the Alt-Tab order, never getting silently dropped or front-pushed.
#[test]
fn adopt_of_unfocused_window_keeps_focus_history() {
    let tmp = TempDir::new();
    let mut f = Fixture::with_config(Config::default());
    f.add_output(1, (1920, 1080));
    inject_cache(&mut f, &tmp, &["myapp"]);
    origin_view(&mut f);

    // A window of the app opens (focused on map); then B opens and takes focus, so A is
    // unfocused and sits behind B in the MRU: order is [B, A].
    let cid = f.add_client();
    let a = map_window(&mut f, cid, "myapp", (300, 200));
    let bid = f.add_client();
    let b = map_window(&mut f, bid, "other", (300, 200));
    let a_win = window_by_app_id(&mut f, "myapp").unwrap();
    let b_win = window_by_app_id(&mut f, "other").unwrap();
    assert_eq!(
        f.state().focused_window().as_ref(),
        Some(&b_win),
        "B holds focus, A is unfocused"
    );
    let order_before = mru_client_order(&mut f);
    assert_eq!(
        order_before,
        vec![b_win.clone(), a_win.clone()],
        "MRU is [B, A] before adoption — A trails B"
    );

    // Relaunch a same-app stand-in; unfocused A forwards the token.
    let sid = insert_suspended(&mut f, 1, "myapp", (800, 500), (400, 300));
    f.state().relaunch_suspended(sid);
    let token = f.state().pending_relaunch_token_for_test(sid).unwrap();
    present_token(&mut f, cid, &a, token);

    let adopted = window_by_app_id(&mut f, "myapp").expect("A adopted the slot");
    assert_eq!(
        f.state().focused_window().as_ref(),
        Some(&b_win),
        "focus stayed on B — the non-refocus adopt path ran"
    );
    assert_eq!(
        mru_client_order(&mut f),
        vec![b_win, adopted],
        "the adopted window kept A's exact MRU slot (behind B), not front-pushed or dropped"
    );

    settle_resize(&mut f, cid, &a, (400, 300));
    client_close(&mut f, cid, &a);
    client_close(&mut f, bid, &b);
}

/// A window under an active interactive move grab must NOT be adopted mid-drag:
/// teleporting it into the stand-in slot would fight the live grab. The token
/// stashes the adopt — leaving the pending relaunch live and the stand-in intact
/// — rather than dismissing, and the drag's end lands it off the stash alone,
/// without the app presenting the token again.
#[test]
fn mid_move_grab_defers_adoption_then_adopts_when_it_ends() {
    let tmp = TempDir::new();
    let mut f = Fixture::with_config(Config::default());
    f.add_output(1, (1920, 1080));
    inject_cache(&mut f, &tmp, &["myapp"]);
    origin_view(&mut f);

    // An existing window of the app is open; a same-app stand-in is relaunched.
    let cid = f.add_client();
    let existing = map_window(&mut f, cid, "myapp", (300, 200));
    let win = window_by_app_id(&mut f, "myapp").unwrap();
    let pos_before = f.state().stage.position_of(&win);

    let sid = insert_suspended(&mut f, 1, "myapp", (800, 500), (400, 300));
    let susp = StageWindow::Suspended(f.state().find_suspended(sid).unwrap());
    let eid = f.state().stage.id_of(&susp).unwrap();
    f.state().relaunch_suspended(sid);
    f.state().expire_relaunch_fallback_for_test(sid);
    let token = f.state().pending_relaunch_token_for_test(sid).unwrap();

    // The window is under a live interactive move grab; the token arrives.
    f.state().arm_interactive_move(&win);
    present_token(&mut f, cid, &existing, token);

    // Defer, not adopt: the window stayed put, the stand-in and pending survive.
    assert_eq!(
        f.state().stage.position_of(&win),
        pos_before,
        "the window was not teleported out from under its grab"
    );
    assert!(
        suspended_present(&mut f),
        "the stand-in was retained, not dismissed"
    );
    assert_eq!(
        f.state().debug_counters()["pending_relaunches"],
        1,
        "the pending relaunch stays live for its TTL"
    );
    assert_eq!(token_count(&mut f), 1, "the token was not deregistered");
    assert_eq!(
        f.state().debug_counters()["deferred_adoptions"],
        1,
        "the adopt was stashed for the grab's release"
    );

    // The drag ends: the stash alone lands the adopt.
    f.state().disarm_interactive_move(&win);
    f.pump(1);

    let adopted = window_by_app_id(&mut f, "myapp").expect("adopted once the grab ended");
    assert_eq!(
        f.state().stage.id_of(&adopted),
        Some(eid),
        "took the stand-in's ElementId"
    );
    assert_eq!(
        f.state().stage.position_of(&adopted),
        Some(Point::from((800, 500))),
        "relocated onto the stand-in rect after the grab cleared"
    );
    assert!(
        !suspended_present(&mut f),
        "the stand-in was consumed by the adopt"
    );

    settle_resize(&mut f, cid, &existing, (400, 300));
    client_close(&mut f, cid, &existing);
}

/// The first-commit path must not adopt into a stand-in the user is dragging:
/// the adopt destroys the stand-in, leaving the grab that was driving it pushing
/// air. The relaunched window takes normal placement instead — a state it can
/// sit in indefinitely — and the stashed adopt lands the moment the drag ends,
/// without the app committing or activating again.
#[test]
fn first_commit_adopt_defers_under_a_stand_in_drag_then_lands_on_release() {
    use smithay::reexports::wayland_server::Resource;

    let tmp = TempDir::new();
    let mut f = Fixture::with_config(Config::default());
    f.add_output(1, (1920, 1080));
    inject_cache(&mut f, &tmp, &["myapp"]);
    origin_view(&mut f);

    let sid = insert_suspended(&mut f, 1, "myapp", (800, 500), (400, 300));
    let susp = StageWindow::Suspended(f.state().find_suspended(sid).unwrap());
    let eid = f.state().stage.id_of(&susp).unwrap();
    f.state().relaunch_suspended(sid);
    let token = f.state().pending_relaunch_token_for_test(sid).unwrap();

    // The user grabs the stand-in while the app is still starting up.
    f.state().arm_interactive_move(&sid);

    // The relaunched app maps, presents its token, and reaches the first sized
    // commit — the adopt point.
    let cid = f.add_client();
    let surface = begin_window(&mut f, cid, "myapp");
    present_token(&mut f, cid, &surface, token);
    finish_window(&mut f, cid, &surface, (300, 200));

    let placed = mapped_client(&mut f, "myapp").expect("the window mapped");
    assert!(
        suspended_present(&mut f),
        "the stand-in survived the commit that would have consumed it"
    );
    assert_ne!(
        f.state().stage.id_of(&placed),
        Some(eid),
        "the window was placed on its own, not seated in the dragged stand-in's slot"
    );
    assert!(
        f.state().camera_target().is_none(),
        "the placement staged no camera flight: a pan warps the pointer into the live grab"
    );
    assert_eq!(
        f.state().debug_counters()["deferred_adoptions"],
        1,
        "the adopt was stashed for the grab's release"
    );
    assert_eq!(
        f.state().debug_counters()["pending_relaunches"],
        1,
        "the pending relaunch stays live for its TTL"
    );
    assert_eq!(token_count(&mut f), 1, "the token was not deregistered");

    // The drag ends: the adopt lands off the release alone.
    f.state().disarm_interactive_move(&sid);
    f.pump(1);

    let adopted = window_by_app_id(&mut f, "myapp").expect("adopted once the grab ended");
    assert_eq!(
        f.state().stage.id_of(&adopted),
        Some(eid),
        "took the stand-in's ElementId"
    );
    assert_eq!(
        f.state().stage.position_of(&adopted),
        Some(Point::from((800, 500))),
        "relocated onto the stand-in rect after the grab cleared"
    );
    assert!(
        !suspended_present(&mut f),
        "the stand-in was consumed by the adopt"
    );
    assert_eq!(f.state().debug_counters()["deferred_adoptions"], 0);

    // The deferral had the window take normal placement, which cached a stable
    // snap rect at that site; the adopt that follows must not inherit it, or the
    // window is carrying a footprint from a slot it no longer occupies.
    let adopted_id = server_surface(&adopted).id();
    assert!(
        !f.state().stable_snap_rects.contains_key(&adopted_id),
        "the rect the normal placement cached left with the adopt"
    );
    assert!(
        f.state().pending_adopt_settle.contains_key(&adopted_id),
        "the adopt owes one until the client draws the body size"
    );

    settle_resize(&mut f, cid, &surface, (400, 300));
    client_close(&mut f, cid, &surface);
}

/// The token path defers on the same grab, read from the other side: the window
/// presenting the token is idle, and it is the *stand-in* the user is dragging.
/// Adopting would still destroy it mid-drag, so the adopt waits for the release
/// — and lands there without the app presenting the token a second time.
#[test]
fn token_adopt_defers_under_a_stand_in_drag_then_lands_on_release() {
    let tmp = TempDir::new();
    let mut f = Fixture::with_config(Config::default());
    f.add_output(1, (1920, 1080));
    inject_cache(&mut f, &tmp, &["myapp"]);
    origin_view(&mut f);

    // An existing window of the app is open; a same-app stand-in is relaunched.
    let cid = f.add_client();
    let existing = map_window(&mut f, cid, "myapp", (300, 200));
    let win = window_by_app_id(&mut f, "myapp").unwrap();
    let pos_before = f.state().stage.position_of(&win);

    let sid = insert_suspended(&mut f, 1, "myapp", (800, 500), (400, 300));
    let susp = StageWindow::Suspended(f.state().find_suspended(sid).unwrap());
    let eid = f.state().stage.id_of(&susp).unwrap();
    f.state().relaunch_suspended(sid);
    let token = f.state().pending_relaunch_token_for_test(sid).unwrap();

    // The stand-in, not the window, is the one under the live grab.
    f.state().arm_interactive_move(&sid);
    present_token(&mut f, cid, &existing, token);

    assert_eq!(
        f.state().stage.position_of(&win),
        pos_before,
        "the window was not teleported into the dragged slot"
    );
    assert!(
        suspended_present(&mut f),
        "the stand-in was retained, not dismissed"
    );
    assert_eq!(
        f.state().debug_counters()["deferred_adoptions"],
        1,
        "the adopt was stashed for the grab's release"
    );
    assert_eq!(
        f.state().debug_counters()["pending_relaunches"],
        1,
        "the pending relaunch stays live for its TTL"
    );
    assert_eq!(token_count(&mut f), 1, "the token was not deregistered");

    f.state().disarm_interactive_move(&sid);
    f.pump(1);

    let adopted = window_by_app_id(&mut f, "myapp").expect("adopted once the drag ended");
    assert_eq!(
        f.state().stage.id_of(&adopted),
        Some(eid),
        "took the stand-in's ElementId"
    );
    assert_eq!(
        f.state().stage.position_of(&adopted),
        Some(Point::from((800, 500))),
        "relocated onto the stand-in rect after the grab cleared"
    );
    assert!(
        !suspended_present(&mut f),
        "the stand-in was consumed by the adopt"
    );

    settle_resize(&mut f, cid, &existing, (400, 300));
    client_close(&mut f, cid, &existing);
}

/// A drag that outlives the 30s relaunch deadline is the deferral's end state:
/// the deadline sweep reclaims the pending relaunch, the next tick's liveness
/// sweep drops the stashed adopt with it, and the window keeps the placement it
/// was given while the stand-in stays behind as a stale duplicate — exactly what
/// an app that took longer than the TTL to come back leaves behind.
#[test]
fn an_adopt_deferred_past_the_relaunch_deadline_leaves_a_stale_stand_in() {
    let tmp = TempDir::new();
    let mut f = Fixture::with_config(Config::default());
    f.add_output(1, (1920, 1080));
    inject_cache(&mut f, &tmp, &["myapp"]);
    origin_view(&mut f);

    let sid = insert_suspended(&mut f, 1, "myapp", (800, 500), (400, 300));
    let susp = StageWindow::Suspended(f.state().find_suspended(sid).unwrap());
    let eid = f.state().stage.id_of(&susp).unwrap();
    f.state().relaunch_suspended(sid);
    let token = f.state().pending_relaunch_token_for_test(sid).unwrap();

    f.state().arm_interactive_move(&sid);
    let cid = f.add_client();
    let surface = begin_window(&mut f, cid, "myapp");
    present_token(&mut f, cid, &surface, token);
    finish_window(&mut f, cid, &surface, (300, 200));
    assert_eq!(f.state().debug_counters()["deferred_adoptions"], 1);

    // The drag is still going when the deadline passes.
    f.state()
        .sweep_pending_relaunches(Instant::now() + Duration::from_secs(31));
    assert_eq!(f.state().debug_counters()["pending_relaunches"], 0);

    // The very next tick reclaims the stash: an entry whose relaunch is gone can
    // no longer land, and the window must not wait on a release for that.
    f.pump(1);
    assert_eq!(
        f.state().debug_counters()["deferred_adoptions"],
        0,
        "the liveness sweep drained the stash without waiting for the release"
    );

    f.state().disarm_interactive_move(&sid);
    f.pump(1);

    assert!(
        suspended_present(&mut f),
        "the stand-in stays behind as a stale duplicate"
    );
    let placed = mapped_client(&mut f, "myapp").expect("the window kept its own placement");
    assert_ne!(
        f.state().stage.id_of(&placed),
        Some(eid),
        "the expired relaunch was not revived into an adopt"
    );

    f.state().dismiss_suspended(sid);
    client_close(&mut f, cid, &surface);
}

/// A second presentation of the token while the deferral is still outstanding is
/// idempotent: one window can only ever adopt one stand-in, so the stash holds a
/// single entry for that surface and the release lands a single adopt. (The grab
/// is still live throughout, so no flush can pre-empt the re-presentation.)
#[test]
fn a_token_re_presented_under_the_grab_stays_one_deferred_adopt() {
    let tmp = TempDir::new();
    let mut f = Fixture::with_config(Config::default());
    f.add_output(1, (1920, 1080));
    inject_cache(&mut f, &tmp, &["myapp"]);
    origin_view(&mut f);

    let cid = f.add_client();
    let existing = map_window(&mut f, cid, "myapp", (300, 200));
    let win = window_by_app_id(&mut f, "myapp").unwrap();
    let pos_before = f.state().stage.position_of(&win);

    let sid = insert_suspended(&mut f, 1, "myapp", (800, 500), (400, 300));
    let susp = StageWindow::Suspended(f.state().find_suspended(sid).unwrap());
    let eid = f.state().stage.id_of(&susp).unwrap();
    f.state().relaunch_suspended(sid);
    let token = f.state().pending_relaunch_token_for_test(sid).unwrap();

    f.state().arm_interactive_move(&win);
    present_token(&mut f, cid, &existing, token.clone());
    present_token(&mut f, cid, &existing, token);

    assert_eq!(
        f.state().debug_counters()["deferred_adoptions"],
        1,
        "the re-presented token replaced the stash rather than stacking a second entry"
    );
    assert_eq!(
        f.state().stage.position_of(&win),
        pos_before,
        "neither presentation teleported the window out from under its grab"
    );
    assert!(suspended_present(&mut f), "the stand-in was retained");
    assert_eq!(token_count(&mut f), 1, "the token was not deregistered");

    f.state().disarm_interactive_move(&win);
    f.pump(1);

    let adopted = window_by_app_id(&mut f, "myapp").expect("adopted once the grab ended");
    assert_eq!(
        f.state().stage.id_of(&adopted),
        Some(eid),
        "took the stand-in's ElementId"
    );
    assert_eq!(
        f.state().debug_counters()["deferred_adoptions"],
        0,
        "the single stash entry drained"
    );
    assert!(!suspended_present(&mut f));

    settle_resize(&mut f, cid, &existing, (400, 300));
    client_close(&mut f, cid, &existing);
}

/// One drive of the assertion below. The hooks are `fn` pointers rather than
/// closures because no case needs to capture anything.
struct DeferredAdoptCase<'a> {
    rules: &'a str,
    /// Runs after the token is presented, before the first sized commit.
    before_first_commit: fn(&mut Fixture, ClientId, &ClientSurface),
    /// `Some(size)` when `rules` forces one and only starts matching at the
    /// first sized commit: the rule configures there and defers the rest of
    /// placement to the client's follow-up commit at that size, which runs the
    /// whole block a second time. (A rule that already matched at the initial
    /// zero-size commit spends its one-shot there instead, so the sized commit
    /// is the only pass.)
    size_pass: Option<(u16, u16)>,
    /// Runs between the two placement passes, where the surface is back in
    /// `pending_center` and a client request queues instead of applying.
    before_size_pass: fn(&mut Fixture, ClientId, &ClientSurface),
    /// Set when `rules` makes the window a widget, which leaves the camera
    /// assertion below no work: the whole navigate block is skipped for a
    /// widget whatever the deferral says, so the flight it guards against
    /// cannot be staged in the first place.
    widget: bool,
}

impl Default for DeferredAdoptCase<'_> {
    fn default() -> Self {
        Self {
            rules: "",
            before_first_commit: |_, _, _| {},
            size_pass: None,
            before_size_pass: |_, _, _| {},
            widget: false,
        }
    }
}

/// The first-commit path resolves adoption *ahead* of window rules and of the
/// fullscreen/fit a client can queue before its first buffer, so an adopt it
/// deferred under a stand-in drag must still beat them when it lands —
/// otherwise something the user never aimed at the stand-in silently destroys
/// the thing they are holding. Drives one case end to end: relaunch, grab the
/// stand-in, let the app reach its first sized commit under the grab (and, for
/// a size rule, the second placement pass that commit sets up), release.
fn assert_a_deferred_first_commit_adopt_wins(case: DeferredAdoptCase) {
    let tmp = TempDir::new();
    let mut f = Fixture::with_config(config(case.rules));
    f.add_output(1, (1920, 1080));
    inject_cache(&mut f, &tmp, &["myapp"]);
    origin_view(&mut f);

    let sid = insert_suspended(&mut f, 1, "myapp", (800, 500), (400, 300));
    let susp = StageWindow::Suspended(f.state().find_suspended(sid).unwrap());
    let eid = f.state().stage.id_of(&susp).unwrap();
    f.state().relaunch_suspended(sid);
    // Long enough a drag that the identity fallback has lapsed, so the stashed
    // token is the only thing that can still resolve the adopt.
    f.state().expire_relaunch_fallback_for_test(sid);
    let token = f.state().pending_relaunch_token_for_test(sid).unwrap();

    // The user grabs the stand-in while the app is still starting up.
    f.state().arm_interactive_move(&sid);

    let cid = f.add_client();
    let surface = begin_window(&mut f, cid, "myapp");
    present_token(&mut f, cid, &surface, token);
    (case.before_first_commit)(&mut f, cid, &surface);
    finish_window(&mut f, cid, &surface, (300, 200));
    if let Some(size) = case.size_pass {
        (case.before_size_pass)(&mut f, cid, &surface);
        // Acking the rule's forced size is the second placement pass.
        settle_resize(&mut f, cid, &surface, size);
    }

    let placed = mapped_client(&mut f, "myapp").expect("the window mapped");
    assert!(
        suspended_present(&mut f),
        "the stand-in survived the commit that would have consumed it"
    );
    assert!(
        !f.state().is_window_fullscreen(&placed) && !f.state().is_pinned(&placed),
        "the membership was suppressed for the deferral, not established and then torn down"
    );
    if !case.widget {
        assert!(
            f.state().camera_target().is_none(),
            "the placement staged no camera flight: a pan warps the pointer into the live grab"
        );
    }
    assert_eq!(
        f.state().debug_counters()["deferred_adoptions"],
        1,
        "the adopt was stashed for the grab's release"
    );

    f.state().disarm_interactive_move(&sid);
    f.pump(1);

    let adopted = mapped_client(&mut f, "myapp").expect("the deferred adopt landed");
    assert_eq!(
        f.state().stage.id_of(&adopted),
        Some(eid),
        "took the stand-in's slot — nothing dismissed it at the flush"
    );
    assert_eq!(
        f.state().stage.position_of(&adopted),
        Some(Point::from((800, 500))),
        "relocated onto the stand-in rect the user dragged it to"
    );
    assert!(
        !suspended_present(&mut f),
        "the stand-in was consumed by the adopt, not dismissed"
    );

    settle_resize(&mut f, cid, &surface, (400, 300));
    client_close(&mut f, cid, &surface);
}

#[test]
fn a_fullscreen_rule_loses_to_a_deferred_first_commit_adopt() {
    assert_a_deferred_first_commit_adopt_wins(DeferredAdoptCase {
        rules: r#"
[[window_rules]]
app_id = "myapp"
fullscreen = true
"#,
        ..Default::default()
    });
}

#[test]
fn a_pin_rule_loses_to_a_deferred_first_commit_adopt() {
    assert_a_deferred_first_commit_adopt_wins(DeferredAdoptCase {
        rules: r#"
[[window_rules]]
app_id = "myapp"
pinned_to_screen = true
"#,
        ..Default::default()
    });
}

#[test]
fn a_widget_rule_loses_to_a_deferred_first_commit_adopt() {
    assert_a_deferred_first_commit_adopt_wins(DeferredAdoptCase {
        rules: r#"
[[window_rules]]
app_id = "myapp"
widget = true
"#,
        widget: true,
        ..Default::default()
    });
}

/// A title-matched rule starts applying only once the app names its window,
/// which for many toolkits is the commit that brings the first buffer — so the
/// forced `size` configures *there* and hands the rest of placement to a second
/// pass. That pass can no longer re-derive the adopt (the token stash was spent
/// on the first, the identity fallback has lapsed), so the suppression has to
/// outlive the pass that established it or the rule pins the window after all.
/// `size` + `pinned_to_screen` is a common rule shape.
#[test]
fn a_pin_rule_with_a_size_loses_to_a_deferred_first_commit_adopt() {
    assert_a_deferred_first_commit_adopt_wins(DeferredAdoptCase {
        rules: r#"
[[window_rules]]
title = "ready"
pinned_to_screen = true
size = [500, 400]
"#,
        before_first_commit: name_the_window,
        size_pass: Some((500, 400)),
        ..Default::default()
    });
}

/// The fullscreen rule through the same two passes.
#[test]
fn a_fullscreen_rule_with_a_size_loses_to_a_deferred_first_commit_adopt() {
    assert_a_deferred_first_commit_adopt_wins(DeferredAdoptCase {
        rules: r#"
[[window_rules]]
title = "ready"
fullscreen = true
size = [500, 400]
"#,
        before_first_commit: name_the_window,
        size_pass: Some((500, 400)),
        ..Default::default()
    });
}

/// Give the window the title the size rules above match on, after its initial
/// commit — that is what leaves the rule's one-shot size configure unspent until
/// the first sized commit, and so splits placement across two passes.
fn name_the_window(f: &mut Fixture, cid: ClientId, surface: &ClientSurface) {
    f.client(cid).window(surface).set_title("ready");
    f.roundtrip(cid);
}

/// A client that asks for fullscreen before its first buffer is the same
/// question the `fullscreen` rule asks, from the client's side — video players
/// do exactly this on relaunch.
#[test]
fn a_client_queued_fullscreen_loses_to_a_deferred_first_commit_adopt() {
    assert_a_deferred_first_commit_adopt_wins(DeferredAdoptCase {
        before_first_commit: |f, cid, surface| {
            f.client(cid).window(surface).set_fullscreen(None);
            f.roundtrip(cid);
        },
        ..Default::default()
    });
}

/// The same request landing *between* the two placement passes: the forced-size
/// configure puts the surface back in `pending_center`, so it queues exactly as a
/// pre-first-buffer one does and the second pass would apply it.
#[test]
fn a_fullscreen_queued_between_placement_passes_loses_to_a_deferred_adopt() {
    assert_a_deferred_first_commit_adopt_wins(DeferredAdoptCase {
        rules: r#"
[[window_rules]]
title = "ready"
size = [500, 400]
"#,
        before_first_commit: name_the_window,
        size_pass: Some((500, 400)),
        before_size_pass: |f, cid, surface| {
            f.client(cid).window(surface).set_fullscreen(None);
            f.roundtrip(cid);
        },
        ..Default::default()
    });
}

/// The flush re-runs the whole decision for every stashed entry, and it is
/// scheduled by any grab release anywhere — so it fires while this entry's own
/// grab is still held. A relaunched window that fullscreened itself mid-drag
/// then meets the carve-out, which must not destroy the stand-in still under the
/// user's cursor. Once the drag really ends the carve-out is the right answer.
#[test]
fn a_flush_under_the_live_grab_leaves_the_dragged_stand_in_alone() {
    let tmp = TempDir::new();
    let mut f = Fixture::with_config(Config::default());
    f.add_output(1, (1920, 1080));
    inject_cache(&mut f, &tmp, &["myapp"]);
    // No camera override: output-aligned, the fullscreen park below is a no-op
    // and the blur-generation counter returns to baseline.

    let sid = insert_suspended(&mut f, 1, "myapp", (800, 500), (400, 300));
    f.state().relaunch_suspended(sid);
    let token = f.state().pending_relaunch_token_for_test(sid).unwrap();

    f.state().arm_interactive_move(&sid);
    let cid = f.add_client();
    let surface = begin_window(&mut f, cid, "myapp");
    present_token(&mut f, cid, &surface, token);
    finish_window(&mut f, cid, &surface, (300, 200));
    assert_eq!(f.state().debug_counters()["deferred_adoptions"], 1);

    // The relaunched window fullscreens itself while the drag is still going.
    // The request is queued rather than taken — nothing is drawn for the window,
    // so nothing may be made to fill the screen — and lands at the reveal, where
    // it is the membership the flush answers by dismissing the stand-in.
    f.client(cid).window(&surface).set_fullscreen(None);
    f.roundtrip(cid);
    let placed = mapped_client(&mut f, "myapp").unwrap();
    assert!(
        !f.state().is_window_fullscreen(&placed),
        "precondition: a hidden window takes no fullscreen membership"
    );

    // An unrelated window's move grab ends: that alone schedules the flush.
    let other_cid = f.add_client();
    let other_surface = map_window(&mut f, other_cid, "other", (200, 200));
    let other = window_by_app_id(&mut f, "other").unwrap();
    f.state().arm_interactive_move(&other);
    f.state().disarm_interactive_move(&other);
    f.pump(1);

    assert!(
        suspended_present(&mut f),
        "the stand-in the user is still dragging survived the flush"
    );
    assert_eq!(
        f.state().debug_counters()["deferred_adoptions"],
        1,
        "the entry deferred again instead of resolving under the live grab"
    );

    // The drag ends, so the fullscreen carve-out gets to answer.
    f.state().disarm_interactive_move(&sid);
    f.pump(1);
    assert!(
        !suspended_present(&mut f),
        "with no grab left, a window that went fullscreen drops the stand-in"
    );
    assert!(
        f.state().is_window_fullscreen(&placed),
        "the window kept the fullscreen it asked for"
    );
    assert_eq!(f.state().debug_counters()["deferred_adoptions"], 0);

    client_close(&mut f, other_cid, &other_surface);
    client_close(&mut f, cid, &surface);
}

/// The other half of `element_under_interactive_grab`: a client resize, witnessed
/// by the surface's own `ResizeState` rather than by the move-grab list. Its
/// teardown runs no disarm, so the stash has to be picked up by the commit that
/// settles the resize back to `Idle` — without that hook the adopt is stranded
/// for good.
#[test]
fn an_adopt_deferred_by_a_client_resize_lands_when_the_resize_settles() {
    let tmp = TempDir::new();
    let mut f = Fixture::with_config(Config::default());
    let out = f.add_output(1, (1920, 1080));
    inject_cache(&mut f, &tmp, &["myapp"]);
    origin_view(&mut f);

    let cid = f.add_client();
    let existing = map_window(&mut f, cid, "myapp", (400, 300));
    let win = window_by_app_id(&mut f, "myapp").unwrap();
    f.state().map_window(
        StageWindow::Client(win.clone()),
        Point::from((400, 300)),
        true,
    );

    let sid = insert_suspended(&mut f, 1, "myapp", (800, 500), (400, 300));
    let susp = StageWindow::Suspended(f.state().find_suspended(sid).unwrap());
    let eid = f.state().stage.id_of(&susp).unwrap();
    f.state().relaunch_suspended(sid);
    let token = f.state().pending_relaunch_token_for_test(sid).unwrap();

    // The user is dragging the window's right edge when the token arrives.
    install_client_resize_grab(
        &mut f,
        &win,
        xdg_toplevel::ResizeEdge::Right,
        Point::from((800.0, 450.0)),
        out,
        ClusterResizeSnapshot::empty(),
    );
    present_token(&mut f, cid, &existing, token);

    assert_eq!(
        f.state().debug_counters()["deferred_adoptions"],
        1,
        "the resize half of the grab check deferred the adopt"
    );
    assert!(suspended_present(&mut f), "the stand-in was retained");

    motion(&mut f, Point::from((900.0, 450.0)));
    f.double_roundtrip(cid);
    adopt_last_configure(&mut f, cid, &existing);

    end_grab(&mut f);
    f.pump(1);
    assert_eq!(
        f.state().debug_counters()["deferred_adoptions"],
        1,
        "the grab's release alone leaves it stashed — the surface is still mid-settle"
    );

    // The commit that settles the resize back to Idle is what lets it go.
    f.double_roundtrip(cid);
    adopt_last_configure(&mut f, cid, &existing);
    f.pump(1);

    let adopted = mapped_client(&mut f, "myapp").expect("the deferred adopt landed");
    assert_eq!(
        f.state().stage.id_of(&adopted),
        Some(eid),
        "took the stand-in's ElementId once the resize settled"
    );
    assert_eq!(
        f.state().stage.position_of(&adopted),
        Some(Point::from((800, 500))),
        "relocated onto the stand-in rect"
    );
    assert!(!suspended_present(&mut f), "the stand-in was consumed");
    assert_eq!(f.state().debug_counters()["deferred_adoptions"], 0);

    settle_resize(&mut f, cid, &existing, (400, 300));
    client_close(&mut f, cid, &existing);
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

/// Put a first-commit adopt in the stash: the app comes back while the user is
/// dragging the stand-in it is bound for, so its window is staged and placed at
/// 300x200 but held off the screen until the drag lets go. Leaves the grab
/// armed; the caller ends it (or abandons the adopt) however the scenario asks.
fn hide_under_a_stand_in_drag(f: &mut Fixture, cid: ClientId, sid: SuspendedId) -> ClientSurface {
    f.state().relaunch_suspended(sid);
    let token = f.state().pending_relaunch_token_for_test(sid).unwrap();
    f.state().arm_interactive_move(&sid);
    let surface = begin_window(f, cid, "myapp");
    present_token(f, cid, &surface, token);
    finish_window(f, cid, &surface, (300, 200));
    assert_eq!(
        f.state().debug_counters()["deferred_adoptions"],
        1,
        "precondition: the adopt is stashed, which is what hides the window"
    );
    surface
}

/// Move a hidden window somewhere the scenario can aim at, without disturbing
/// the stash.
fn seat_at(f: &mut Fixture, window: &smithay::desktop::Window, pos: (i32, i32)) {
    f.state()
        .map_window(StageWindow::Client(window.clone()), Point::from(pos), false);
}

/// Release the drag onto nothing: dismissing the stand-in first leaves the
/// deferred adopt with no slot to take, so the release reveals the window at the
/// placement it has been holding rather than teleporting it.
fn release_onto_a_dismissed_stand_in(f: &mut Fixture, sid: SuspendedId) {
    f.state().dismiss_suspended(sid);
    f.state().disarm_interactive_move(&sid);
    f.pump(1);
    assert_eq!(
        f.state().debug_counters()["deferred_adoptions"],
        0,
        "precondition: the stash drained, so the window is revealed"
    );
}

/// A window awaiting a deferred adopt is not drawn where it sits, so nothing may
/// snap or cluster to it there: the flush is about to teleport it, and a drag or
/// a fit that carried it along would move a window the user cannot see. Revealed,
/// it joins the cluster from the same rect.
#[test]
#[allow(clippy::mutable_key_type)]
fn a_hidden_adopt_is_no_cluster_citizen_until_it_is_revealed() {
    let tmp = TempDir::new();
    let mut f = Fixture::with_config(Config::default());
    f.add_output(1, (1920, 1080));
    inject_cache(&mut f, &tmp, &["myapp"]);
    origin_view(&mut f);

    // The dragged stand-in sits well away from the pair below.
    let sid = insert_suspended(&mut f, 1, "myapp", (5000, 5000), (400, 300));

    let nb = f.add_client();
    let nb_surface = map_window(&mut f, nb, "nb", (400, 300));
    let neighbor = window_by_app_id(&mut f, "nb").unwrap();
    let nb_elem = StageWindow::Client(neighbor.clone());
    seat_at(&mut f, &neighbor, (1000, 1000));

    let cid = f.add_client();
    let surface = hide_under_a_stand_in_drag(&mut f, cid, sid);
    let hidden = mapped_client(&mut f, "myapp").unwrap();
    let hidden_elem = StageWindow::Client(hidden.clone());

    // Exactly one snap gap off the neighbor's right edge — a cluster the moment
    // the window counts as a citizen at all.
    let gap = f.state().config.snap_gap as i32;
    let nb_right = f.state().snap_rect_for(&nb_elem).unwrap().x_high as i32;
    let bw = f.state().element_border_width(&hidden_elem);
    seat_at(&mut f, &hidden, (nb_right + gap + bw, 1000));

    let clustered = |f: &mut Fixture, w: &StageWindow| {
        let rects = f.state().all_windows_with_snap_rects();
        driftwm::layout::cluster::cluster_of(&nb_elem, &rects, f.state().config.snap_gap)
            .contains(w)
    };
    assert!(
        !clustered(&mut f, &hidden_elem),
        "a window nothing is drawn for must not be in the neighbor's cluster"
    );

    release_onto_a_dismissed_stand_in(&mut f, sid);

    assert!(
        clustered(&mut f, &hidden_elem),
        "the revealed window clusters from the very rect that was ignored while it was hidden"
    );

    client_close(&mut f, cid, &surface);
    client_close(&mut f, nb, &nb_surface);
}

/// The placement that maps a deferred adopt takes no focus and enters no MRU
/// history: the window is not on screen, so Alt-Tab and the keyboard must not
/// reach it. Both are handed over at the reveal — and the adopt that follows
/// needs the history entry, since it restores the window's slot from it.
#[test]
fn a_hidden_adopt_takes_no_focus_or_history_until_it_is_revealed() {
    let tmp = TempDir::new();
    let mut f = Fixture::with_config(Config::default());
    f.add_output(1, (1920, 1080));
    inject_cache(&mut f, &tmp, &["myapp"]);
    origin_view(&mut f);

    // Somebody else holds focus while the app comes back.
    let other = f.add_client();
    let other_surface = map_window(&mut f, other, "other", (200, 200));
    let other_window = window_by_app_id(&mut f, "other").unwrap();

    let sid = insert_suspended(&mut f, 1, "myapp", (800, 500), (400, 300));
    let cid = f.add_client();
    let surface = hide_under_a_stand_in_drag(&mut f, cid, sid);
    let hidden = mapped_client(&mut f, "myapp").unwrap();
    let hidden_elem = StageWindow::Client(hidden.clone());

    assert_eq!(
        f.state().focused_window(),
        Some(other_window),
        "focus stayed with the window the user can actually see"
    );
    assert!(
        !f.state().stage.focus_history().contains(&hidden_elem),
        "an invisible window has no business in the Alt-Tab cycle"
    );

    f.state().disarm_interactive_move(&sid);
    f.pump(1);

    let adopted = mapped_client(&mut f, "myapp").expect("the adopt landed");
    assert_eq!(
        f.state().focused_window(),
        Some(adopted.clone()),
        "the reveal hands over the focus the placement withheld"
    );
    assert!(
        f.state()
            .stage
            .focus_history()
            .contains(&StageWindow::Client(adopted)),
        "and the history entry the adopt reads to restore its slot"
    );

    settle_resize(&mut f, cid, &surface, (400, 300));
    client_close(&mut f, cid, &surface);
    client_close(&mut f, other, &other_surface);
}

/// Same for the `Activated` hint, which is the chrome half of the same thing: a
/// window nobody can see must not be the one wearing the focused decoration.
#[test]
fn a_hidden_adopt_is_not_activated_until_it_is_revealed() {
    let tmp = TempDir::new();
    let mut f = Fixture::with_config(Config::default());
    f.add_output(1, (1920, 1080));
    inject_cache(&mut f, &tmp, &["myapp"]);
    origin_view(&mut f);

    let sid = insert_suspended(&mut f, 1, "myapp", (800, 500), (400, 300));
    let cid = f.add_client();
    let surface = hide_under_a_stand_in_drag(&mut f, cid, sid);
    let hidden = mapped_client(&mut f, "myapp").unwrap();

    assert!(
        !is_activated(&hidden),
        "the placement staged no activation for a window it did not show"
    );

    f.state().disarm_interactive_move(&sid);
    f.pump(1);

    let adopted = mapped_client(&mut f, "myapp").expect("the adopt landed");
    assert!(
        is_activated(&adopted),
        "the reveal activates it, so the slot it lands in wears the focused chrome"
    );

    settle_resize(&mut f, cid, &surface, (400, 300));
    client_close(&mut f, cid, &surface);
}

/// Nothing is drawn for a hidden adopt, so no canvas-space walk may resolve to
/// it: the pointer over its rect belongs to whatever really is under the cursor.
/// All three walks answer independently — `element_under` (focus, bindings),
/// `topmost_under` (gestures, drags) and `surface_under` (the foundation) — and
/// each has its own skip.
#[test]
fn no_canvas_hit_test_walk_reaches_a_hidden_adopt() {
    let tmp = TempDir::new();
    let mut f = Fixture::with_config(Config::default());
    f.add_output(1, (1920, 1080));
    inject_cache(&mut f, &tmp, &["myapp"]);
    origin_view(&mut f);

    let sid = insert_suspended(&mut f, 1, "myapp", (5000, 5000), (400, 300));

    let under = f.add_client();
    let under_surface = map_window(&mut f, under, "under", (400, 300));
    let below = window_by_app_id(&mut f, "under").unwrap();
    seat_at(&mut f, &below, (1000, 1000));

    let cid = f.add_client();
    let surface = hide_under_a_stand_in_drag(&mut f, cid, sid);
    let hidden = mapped_client(&mut f, "myapp").unwrap();
    // Straight over the lower window, and covered by the hidden one on top.
    seat_at(&mut f, &hidden, (1000, 1000));
    let p = Point::from((1100.0, 1050.0));

    assert_eq!(
        f.state().element_under(p).map(|(w, _)| w.clone()),
        Some(below.clone()),
        "element_under answers with the window that is really drawn there"
    );
    assert_eq!(
        f.state().topmost_client_under(p),
        Some(below.clone()),
        "topmost_under too — a hidden window is a skip, never a stop"
    );
    assert_eq!(
        f.state().surface_under(p, None).map(|(t, _)| t.0),
        Some(server_surface(&below)),
        "and surface_under, the foundation the other two are checked against"
    );

    release_onto_a_dismissed_stand_in(&mut f, sid);

    assert_eq!(
        f.state().element_under(p).map(|(w, _)| w.clone()),
        Some(hidden.clone()),
        "revealed in the same place, it takes the point back from the window below"
    );
    assert_eq!(f.state().topmost_client_under(p), Some(hidden.clone()));
    assert_eq!(
        f.state().surface_under(p, None).map(|(t, _)| t.0),
        Some(server_surface(&hidden))
    );

    client_close(&mut f, cid, &surface);
    client_close(&mut f, under, &under_surface);
}

/// The decoration channel is its own z-order walk, and its occlusion stop makes
/// omission worse than a miss: a hidden window covering the point would end the
/// walk with `None` and swallow the click meant for the chrome underneath it.
#[test]
fn a_hidden_adopt_does_not_swallow_a_click_on_the_chrome_beneath_it() {
    use crate::decorations::DecorationHit;
    use crate::input::DecoTarget;

    let tmp = TempDir::new();
    let mut f = Fixture::with_config(Config::default());
    f.add_output(1, (1920, 1080));
    inject_cache(&mut f, &tmp, &["myapp"]);
    origin_view(&mut f);

    let sid = insert_suspended(&mut f, 1, "myapp", (5000, 5000), (400, 300));

    let under = f.add_client();
    let under_surface = map_window(&mut f, under, "under", (400, 300));
    let below = window_by_app_id(&mut f, "under").unwrap();
    seat_at(&mut f, &below, (1000, 1000));

    let cid = f.add_client();
    let surface = hide_under_a_stand_in_drag(&mut f, cid, sid);
    let hidden = mapped_client(&mut f, "myapp").unwrap();
    // The hidden window's *content* covers a point in the lower window's CSD
    // resize margin, which is outside the lower window's own rect.
    seat_at(&mut f, &hidden, (1380, 1050));
    let p = Point::from((1404.0, 1100.0));

    assert!(
        matches!(
            f.state().decoration_under(p),
            Some((DecoTarget::Client(w), DecorationHit::ResizeBorder(_))) if w == below
        ),
        "the resize margin still belongs to the window that draws it"
    );

    release_onto_a_dismissed_stand_in(&mut f, sid);

    assert!(
        f.state().decoration_under(p).is_none(),
        "revealed, its content occludes that margin — which is what makes the \
         assertion above about the hiding and not about the geometry"
    );

    client_close(&mut f, cid, &surface);
    client_close(&mut f, under, &under_surface);
}

/// `center-window` with nothing focused centers the nearest canvas element, and
/// suppressing a deferred adopt's focus makes that the common arm — so without
/// the canvas-eligibility skip a single keypress flies the camera to an
/// invisible window. The stand-in the user is dragging is the nearest element
/// that can legitimately answer.
#[test]
fn center_window_never_flies_the_camera_to_a_hidden_adopt() {
    let tmp = TempDir::new();
    let mut f = Fixture::with_config(Config::default());
    f.add_output(1, (1920, 1080));
    inject_cache(&mut f, &tmp, &["myapp"]);
    origin_view(&mut f);
    // The flight below pans the camera, which populates blur_camera_generation
    // (it drains only on output disconnect) — end off-baseline like the
    // camera-animation suite.
    f.skip_baseline_check();

    let sid = insert_suspended(&mut f, 1, "myapp", (5000, 5000), (400, 300));
    let cid = f.add_client();
    let surface = hide_under_a_stand_in_drag(&mut f, cid, sid);
    let hidden = mapped_client(&mut f, "myapp").unwrap();

    // Straddling the viewport center, so distance alone would elect it.
    let vc = f.state().viewport_center_canvas();
    seat_at(&mut f, &hidden, (vc.x as i32 - 150, vc.y as i32 - 100));
    assert!(
        f.state().focused_element().is_none(),
        "precondition: nothing is focused, so the nearest-element arm answers"
    );

    f.state().execute_action(&Action::CenterWindow);

    let target = f
        .state()
        .camera_target()
        .expect("the camera flew to the stand-in, the only element it may center");
    assert!(
        target.x > 3000.0 && target.y > 3000.0,
        "the flight went to the far stand-in, not the invisible window over the \
         viewport center: {target:?}"
    );

    f.state().disarm_interactive_move(&sid);
    f.pump(1);
    settle_resize(&mut f, cid, &surface, (400, 300));
    client_close(&mut f, cid, &surface);
}

/// A hidden adopt occupies no visible ground, so it reserves none: auto
/// placement must put a new window exactly where it would have with the hidden
/// one absent. Reserving that ground would push new windows off into free canvas
/// to dodge something nobody can see — and the flush is about to move it anyway.
#[test]
fn auto_placement_reserves_no_ground_for_a_hidden_adopt() {
    let tmp = TempDir::new();
    let mut f = Fixture::with_config(Config::default());
    f.add_output(1, (1920, 1080));
    inject_cache(&mut f, &tmp, &["myapp"]);
    origin_view(&mut f);

    let sid = insert_suspended(&mut f, 1, "myapp", (5000, 5000), (400, 300));

    let anchor_cid = f.add_client();
    let anchor_surface = map_window(&mut f, anchor_cid, "anchor", (400, 300));
    let anchor = window_by_app_id(&mut f, "anchor").unwrap();
    seat_at(&mut f, &anchor, (900, 500));
    let anchor_elem = StageWindow::Client(anchor.clone());

    // Same size as the hidden window, so seating it on the chosen slot blocks
    // that slot exactly.
    let placing_cid = f.add_client();
    let placing_surface = map_window(&mut f, placing_cid, "placing", (300, 200));
    let placing = window_by_app_id(&mut f, "placing").unwrap();

    let cid = f.add_client();
    let surface = hide_under_a_stand_in_drag(&mut f, cid, sid);
    let hidden = mapped_client(&mut f, "myapp").unwrap();

    let auto_pos = |f: &mut Fixture, placing: &smithay::desktop::Window| {
        let s = server_surface(placing);
        f.state()
            .auto_anchor_snapshot
            .insert(s, Some(anchor_elem.clone()));
        let chrome = f
            .state()
            .element_chrome(&StageWindow::Client(placing.clone()));
        f.state()
            .auto_placement_pos(placing, Size::from((300, 200)), chrome)
    };

    let slot = auto_pos(&mut f, &placing).expect("auto placement docks beside the anchor");
    // Park the hidden window on exactly that slot and ask again.
    seat_at(&mut f, &hidden, slot);
    assert_eq!(
        auto_pos(&mut f, &placing),
        Some(slot),
        "the slot is still free: nothing is drawn on it"
    );

    release_onto_a_dismissed_stand_in(&mut f, sid);

    assert_ne!(
        auto_pos(&mut f, &placing),
        Some(slot),
        "revealed, it does hold the slot — which is what makes the assertion \
         above about the hiding and not about the search order"
    );

    client_close(&mut f, cid, &surface);
    client_close(&mut f, placing_cid, &placing_surface);
    client_close(&mut f, anchor_cid, &anchor_surface);
}

/// A stashed adopt can outlive every grab there is — the drag may simply never
/// end, and a client-resize deferral holds one with no grab live at all — so
/// nothing but the per-frame liveness sweep bounds how long its window stays
/// hidden. Once the relaunch deadline lapses the sweep drops the entry and shows
/// the window where it stands, fully set up: hit-testable, back in the Alt-Tab
/// cycle, and carrying the settled snap rect its suppressed placement never
/// wrote.
#[test]
fn the_liveness_sweep_reveals_an_adopt_whose_relaunch_lapsed() {
    use smithay::reexports::wayland_server::Resource;

    let tmp = TempDir::new();
    let mut f = Fixture::with_config(Config::default());
    f.add_output(1, (1920, 1080));
    inject_cache(&mut f, &tmp, &["myapp"]);
    origin_view(&mut f);

    let sid = insert_suspended(&mut f, 1, "myapp", (5000, 5000), (400, 300));
    let cid = f.add_client();
    let surface = hide_under_a_stand_in_drag(&mut f, cid, sid);
    let hidden = mapped_client(&mut f, "myapp").unwrap();
    seat_at(&mut f, &hidden, (1000, 1000));

    // The drag is still going when the deadline passes.
    f.state()
        .sweep_pending_relaunches(Instant::now() + Duration::from_secs(31));
    f.pump(1);

    assert_eq!(
        f.state().debug_counters()["deferred_adoptions"],
        0,
        "the sweep dropped an entry no release could ever land"
    );
    let p = Point::from((1100.0, 1050.0));
    assert_eq!(
        f.state().element_under(p).map(|(w, _)| w.clone()),
        Some(hidden.clone()),
        "the window is on screen and answers for the pointer over it"
    );
    assert!(
        f.state()
            .stage
            .focus_history()
            .contains(&StageWindow::Client(hidden.clone())),
        "and is back in the Alt-Tab cycle its placement kept it out of"
    );
    assert!(
        f.state()
            .stable_snap_rects
            .contains_key(&server_surface(&hidden).id()),
        "the reveal wrote the settled rect nothing else on this path ever would"
    );

    f.state().disarm_interactive_move(&sid);
    f.pump(1);
    f.state().dismiss_suspended(sid);
    client_close(&mut f, cid, &surface);
}

/// Two windows of one app can both bind to one stand-in — the second matches on
/// the identity fallback while the first's token stash is spent — and the adopt
/// that lands cancels the pending relaunch directly, orphaning the loser. The
/// drain must set that window up as carefully as the winner: it keeps the
/// placement it was given, so it needs the rect and the focus its suppressed
/// placement pass never wrote.
#[test]
fn the_loser_of_a_two_window_race_is_revealed_where_it_stands() {
    use smithay::reexports::wayland_server::Resource;

    let tmp = TempDir::new();
    let mut f = Fixture::with_config(Config::default());
    f.add_output(1, (1920, 1080));
    inject_cache(&mut f, &tmp, &["myapp"]);
    origin_view(&mut f);

    let sid = insert_suspended(&mut f, 1, "myapp", (800, 500), (400, 300));
    let susp = StageWindow::Suspended(f.state().find_suspended(sid).unwrap());
    let eid = f.state().stage.id_of(&susp).unwrap();

    let winner_cid = f.add_client();
    let winner_surface = hide_under_a_stand_in_drag(&mut f, winner_cid, sid);
    let winner = mapped_client(&mut f, "myapp").expect("the token holder mapped");

    // A second window of the same app, token-less: the identity fallback binds
    // it to the same still-pending relaunch.
    let loser_cid = f.add_client();
    let loser_surface = begin_window(&mut f, loser_cid, "myapp");
    finish_window(&mut f, loser_cid, &loser_surface, (300, 200));
    assert_eq!(
        f.state().debug_counters()["deferred_adoptions"],
        2,
        "precondition: both windows are stashed against the one stand-in"
    );
    let loser = f
        .state()
        .stage
        .windows()
        .filter_map(|w| w.client())
        .find(|w| w.app_id_or_class().as_deref() == Some("myapp") && **w != winner)
        .cloned()
        .expect("the second window mapped");
    seat_at(&mut f, &loser, (2000, 2000));

    f.state().disarm_interactive_move(&sid);
    f.pump(1);

    assert_eq!(
        f.state().debug_counters()["deferred_adoptions"],
        0,
        "both entries drained on the one release"
    );
    assert_eq!(
        f.state().stage.id_of(&StageWindow::Client(winner.clone())),
        Some(eid),
        "the token holder took the stand-in's slot"
    );
    assert_eq!(
        f.state().stage.position_of(&loser),
        Some(Point::from((2000, 2000))),
        "the orphan kept the placement it was given"
    );
    assert!(
        f.state()
            .stable_snap_rects
            .contains_key(&server_surface(&loser).id()),
        "and was set up there rather than left half-initialised"
    );
    let p = Point::from((2100.0, 2050.0));
    assert_eq!(
        f.state().element_under(p).map(|(w, _)| w.clone()),
        Some(loser.clone()),
        "the orphan is on screen, not hidden for an adopt that can never come"
    );

    settle_resize(&mut f, winner_cid, &winner_surface, (400, 300));
    client_close(&mut f, winner_cid, &winner_surface);
    client_close(&mut f, loser_cid, &loser_surface);
}

/// A client that fullscreens itself while it is hidden must not get it there:
/// the render skips the window and the fullscreen cull skips everything else,
/// so the output would draw nothing at all until the drag ended. The request
/// waits in the same queue a pre-first-commit one uses and is applied once the
/// adopt has taken the slot — so the rect the fullscreen exit restores is the
/// one the user dragged the stand-in to.
#[test]
fn a_hidden_adopt_cannot_fullscreen_before_it_is_revealed() {
    let tmp = TempDir::new();
    let mut f = Fixture::with_config(Config::default());
    let output = f.add_output(1, (1920, 1080));
    inject_cache(&mut f, &tmp, &["myapp"]);
    // No camera override: output-aligned, so the fullscreen park below is a
    // no-op and the blur-generation counter returns to baseline.

    let sid = insert_suspended(&mut f, 1, "myapp", (800, 500), (400, 300));
    let cid = f.add_client();
    let surface = hide_under_a_stand_in_drag(&mut f, cid, sid);
    let hidden = mapped_client(&mut f, "myapp").unwrap();

    f.client(cid).window(&surface).set_fullscreen(None);
    f.double_roundtrip(cid);

    assert!(
        !f.state().is_output_fullscreen(&output),
        "a window nothing is drawn for must never own the output's fullscreen"
    );
    assert!(
        !f.state().is_window_fullscreen(&hidden),
        "and must not be carrying the membership either"
    );

    f.state().disarm_interactive_move(&sid);
    f.pump(1);

    assert!(
        f.state().is_window_fullscreen(&hidden),
        "the flush hands over the request the hiding held back"
    );
    let restores = f
        .state()
        .stage
        .fullscreen_on(&output.name())
        .map(|fs| (fs.saved_location, fs.saved_size));
    assert_eq!(
        restores,
        Some((Point::from((800, 500)), Size::from((400, 300)))),
        "and the rect its exit restores is the stand-in's — a fullscreen \
         entered ahead of the adopt saves the holding placement instead, and \
         then reads as policy's own, so the adopt drops the stand-in rather \
         than taking it"
    );

    client_close(&mut f, cid, &surface);
}

/// The maximize twin of the same gate. Milder — a fit pans the camera onto a
/// rect nothing is drawn at rather than blanking the output — but it rides the
/// same queue, and lands when the window is on screen to be fitted.
#[test]
fn a_hidden_adopt_cannot_fit_before_it_is_revealed() {
    let tmp = TempDir::new();
    let mut f = Fixture::with_config(Config::default());
    f.add_output(1, (1920, 1080));
    inject_cache(&mut f, &tmp, &["myapp"]);
    origin_view(&mut f);

    let sid = insert_suspended(&mut f, 1, "myapp", (5000, 5000), (400, 300));
    let cid = f.add_client();
    let surface = hide_under_a_stand_in_drag(&mut f, cid, sid);
    let hidden = mapped_client(&mut f, "myapp").unwrap();

    f.client(cid).window(&surface).set_maximized();
    f.double_roundtrip(cid);

    assert!(
        !f.state().stage.is_fit(&hidden),
        "a hidden window takes no fit membership"
    );

    // Dismissed mid-drag, so the release reveals the window where it stands
    // instead of teleporting it — the fit lands on the rect the user sees.
    release_onto_a_dismissed_stand_in(&mut f, sid);

    assert!(
        f.state().stage.is_fit(&hidden),
        "the reveal applies the queued fit"
    );

    client_close(&mut f, cid, &surface);
}

/// A single-instance app can present its relaunch token a second time on the
/// window it already gave back. That re-stashes the adopt through the activation
/// path, which must not overwrite the first-commit entry: doing so un-hides the
/// window at its holding placement mid-drag, and then the drain skips the reveal
/// that entry is owed, leaving the window out of the Alt-Tab cycle.
#[test]
fn a_second_token_on_a_hidden_window_leaves_it_hidden() {
    let tmp = TempDir::new();
    let mut f = Fixture::with_config(Config::default());
    f.add_output(1, (1920, 1080));
    inject_cache(&mut f, &tmp, &["myapp"]);
    origin_view(&mut f);

    let sid = insert_suspended(&mut f, 1, "myapp", (5000, 5000), (400, 300));

    let under = f.add_client();
    let under_surface = map_window(&mut f, under, "under", (400, 300));
    let below = window_by_app_id(&mut f, "under").unwrap();
    seat_at(&mut f, &below, (1000, 1000));

    let cid = f.add_client();
    let surface = hide_under_a_stand_in_drag(&mut f, cid, sid);
    let hidden = mapped_client(&mut f, "myapp").unwrap();
    seat_at(&mut f, &hidden, (1000, 1000));
    let p = Point::from((1100.0, 1050.0));

    // The stand-in is still under the drag, so the activation path stashes
    // rather than adopting on the spot.
    let token = f.state().pending_relaunch_token_for_test(sid).unwrap();
    present_token(&mut f, cid, &surface, token);

    assert_eq!(
        f.state().debug_counters()["deferred_adoptions"],
        1,
        "the second stash superseded the first rather than piling up"
    );
    assert_eq!(
        f.state().element_under(p).map(|(w, _)| w.clone()),
        Some(below.clone()),
        "the window is still hidden: the point belongs to what is drawn there"
    );

    f.state().disarm_interactive_move(&sid);
    f.pump(1);

    let adopted = mapped_client(&mut f, "myapp").expect("the adopt landed");
    assert!(
        f.state()
            .stage
            .focus_history()
            .contains(&StageWindow::Client(adopted)),
        "and the reveal it was owed still ran, so Alt-Tab can reach it"
    );

    settle_resize(&mut f, cid, &surface, (400, 300));
    client_close(&mut f, cid, &surface);
    client_close(&mut f, under, &under_surface);
}

/// A relaunch that commits while an output is fullscreen is background-placed —
/// tucked behind the fullscreen window, no activation, no focus. The reveal
/// stands in for that placement and has to keep its bargain: raising the window
/// over the fullscreen one and taking the keyboard is exactly what the
/// background arm exists to prevent. Withholding the focus is not withholding
/// the window, though — it still joins the cycle, at the far end.
#[test]
fn a_reveal_behind_a_fullscreen_window_does_not_take_its_focus() {
    use smithay::reexports::wayland_server::Resource;

    let tmp = TempDir::new();
    let mut f = Fixture::with_config(Config::default());
    let output = f.add_output(1, (1920, 1080));
    inject_cache(&mut f, &tmp, &["myapp"]);
    // No camera override: output-aligned, so the fullscreen park below is a
    // no-op and the blur-generation counter returns to baseline.

    let fs = f.add_client();
    let fs_surface = map_window(&mut f, fs, "fs", (400, 300));
    let fs_window = window_by_app_id(&mut f, "fs").unwrap();
    f.client(fs).window(&fs_surface).set_fullscreen(None);
    f.double_roundtrip(fs);
    adopt_last_configure(&mut f, fs, &fs_surface);
    assert!(
        f.state().is_output_fullscreen(&output),
        "precondition: the output is fullscreen, so the relaunch is background-placed"
    );

    let sid = insert_suspended(&mut f, 1, "myapp", (5000, 5000), (400, 300));
    let cid = f.add_client();
    let surface = hide_under_a_stand_in_drag(&mut f, cid, sid);
    let hidden = mapped_client(&mut f, "myapp").unwrap();
    assert_eq!(
        f.state().focused_window(),
        Some(fs_window.clone()),
        "precondition: the fullscreen window still holds the keyboard"
    );

    release_onto_a_dismissed_stand_in(&mut f, sid);

    assert_eq!(
        f.state().focused_window(),
        Some(fs_window.clone()),
        "the reveal left the keyboard with the window filling the screen"
    );
    assert!(
        !is_activated(&hidden),
        "and did not hand it the focused-window chrome hint either"
    );
    let history = f.state().stage.focus_history().to_vec();
    assert_eq!(
        history.first(),
        Some(&StageWindow::Client(fs_window)),
        "precondition: the front of the cycle is still the fullscreen window"
    );
    assert_eq!(
        history.last(),
        Some(&StageWindow::Client(hidden.clone())),
        "but Alt-Tab can reach the revealed window, at the far end where a \
         window that never held focus belongs"
    );
    assert!(
        f.state()
            .stable_snap_rects
            .contains_key(&server_surface(&hidden).id()),
        "the reveal did run — only it writes this rect on a release that lands \
         no adopt — so the assertions above are about the focus, not about \
         nothing happening"
    );

    client_close(&mut f, cid, &surface);
    client_close(&mut f, fs, &fs_surface);
}

/// The liveness sweep can fire a full relaunch deadline after the window was
/// placed, while the user has long since moved on to something else and may
/// still be dragging. Showing the window is the whole point; taking the keyboard
/// out of whatever they are typing into is not.
#[test]
fn the_liveness_sweep_reveal_does_not_take_focus_from_elsewhere() {
    let tmp = TempDir::new();
    let mut f = Fixture::with_config(Config::default());
    f.add_output(1, (1920, 1080));
    inject_cache(&mut f, &tmp, &["myapp"]);
    origin_view(&mut f);

    let other = f.add_client();
    let other_surface = map_window(&mut f, other, "other", (200, 200));
    let other_window = window_by_app_id(&mut f, "other").unwrap();

    let sid = insert_suspended(&mut f, 1, "myapp", (5000, 5000), (400, 300));
    let cid = f.add_client();
    let surface = hide_under_a_stand_in_drag(&mut f, cid, sid);
    let hidden = mapped_client(&mut f, "myapp").unwrap();
    seat_at(&mut f, &hidden, (1000, 1000));

    // The drag never ends; the deadline does.
    f.state()
        .sweep_pending_relaunches(Instant::now() + Duration::from_secs(31));
    f.pump(1);

    assert_eq!(
        f.state()
            .element_under(Point::from((1100.0, 1050.0)))
            .map(|(w, _)| w.clone()),
        Some(hidden.clone()),
        "precondition: the sweep did reveal it — the window is on screen"
    );
    assert_eq!(
        f.state().focused_window(),
        Some(other_window.clone()),
        "the keyboard stayed where the user left it"
    );
    let history = f.state().stage.focus_history().to_vec();
    assert_eq!(
        history.first(),
        Some(&StageWindow::Client(other_window)),
        "precondition: the window the user is working in still fronts the cycle"
    );
    assert_eq!(
        history.last(),
        Some(&StageWindow::Client(hidden)),
        "and Alt-Tab reaches the revealed window at the far end — the half the \
         reveal does owe, without jumping the queue"
    );

    f.state().disarm_interactive_move(&sid);
    f.pump(1);
    f.state().dismiss_suspended(sid);
    client_close(&mut f, cid, &surface);
    client_close(&mut f, other, &other_surface);
}

/// Under `suspend_on_close`, closing a window that is still hidden must not
/// leave a stand-in: the one it was going to be adopted into is still sitting
/// there, so the user would end up with two for one app — the second faded in at
/// a holding placement nobody ever saw the window at.
#[test]
fn closing_a_hidden_adopt_leaves_no_second_stand_in() {
    let tmp = TempDir::new();
    let mut f = Fixture::with_config(config("[session]\nsuspend_on_close = true"));
    f.add_output(1, (1920, 1080));
    inject_cache(&mut f, &tmp, &["myapp"]);
    origin_view(&mut f);

    let sid = insert_suspended(&mut f, 1, "myapp", (800, 500), (400, 300));
    let cid = f.add_client();
    let surface = hide_under_a_stand_in_drag(&mut f, cid, sid);

    client_close(&mut f, cid, &surface);

    assert_eq!(
        f.state()
            .stage
            .windows()
            .filter(|w| w.suspended().is_some())
            .count(),
        1,
        "only the stand-in the user was dragging is left"
    );
    assert_eq!(
        f.state().debug_counters()["deferred_adoptions"],
        0,
        "and the stash went with the surface"
    );

    f.state().disarm_interactive_move(&sid);
    f.state().dismiss_suspended(sid);
}

/// Nothing was released when the liveness sweep fires — the drag that forced the
/// deferral can still be live — so a fullscreen the client asked for while it was
/// hidden must not be handed over there. Taking it would flip the screen to an
/// app the user never saw arrive, pull the keyboard out of whatever they were
/// typing into, and park the camera the drag is still pushing.
#[test]
fn an_abandoned_reveal_does_not_take_the_fullscreen_it_was_holding() {
    let tmp = TempDir::new();
    let mut f = Fixture::with_config(Config::default());
    let output = f.add_output(1, (1920, 1080));
    inject_cache(&mut f, &tmp, &["myapp"]);
    origin_view(&mut f);

    let other = f.add_client();
    let other_surface = map_window(&mut f, other, "other", (200, 200));
    let other_window = window_by_app_id(&mut f, "other").unwrap();

    let sid = insert_suspended(&mut f, 1, "myapp", (5000, 5000), (400, 300));
    let cid = f.add_client();
    let surface = hide_under_a_stand_in_drag(&mut f, cid, sid);
    let hidden = mapped_client(&mut f, "myapp").unwrap();
    seat_at(&mut f, &hidden, (1000, 1000));

    f.client(cid).window(&surface).set_fullscreen(None);
    f.double_roundtrip(cid);

    // The drag never ends; the relaunch deadline does.
    f.state()
        .sweep_pending_relaunches(Instant::now() + Duration::from_secs(31));
    f.pump(1);

    assert!(
        !f.state().is_output_fullscreen(&output),
        "the screen did not flip to an app that turned up mid-drag"
    );
    assert!(
        !f.state().is_window_fullscreen(&hidden),
        "and the window took no fullscreen membership on the way in"
    );
    assert_eq!(
        f.state().focused_window(),
        Some(other_window),
        "so the keyboard stayed where the user left it"
    );
    assert_eq!(
        f.state()
            .element_under(Point::from((1100.0, 1050.0)))
            .map(|(w, _)| w.clone()),
        Some(hidden.clone()),
        "and the sweep did reveal it where it stands, so the three assertions \
         above are about the request, not about nothing having happened"
    );

    f.state().disarm_interactive_move(&sid);
    f.pump(1);
    f.state().dismiss_suspended(sid);
    client_close(&mut f, cid, &surface);
    client_close(&mut f, other, &other_surface);
}

/// A client that fullscreens itself while hidden and then changes its mind must
/// be taken at its second word: the unfullscreen has to reach the queue the
/// first request is waiting in, or the reveal makes the window fullscreen after
/// the client has been told it is not.
#[test]
fn a_hidden_adopt_that_unfullscreens_is_revealed_windowed() {
    let tmp = TempDir::new();
    let mut f = Fixture::with_config(Config::default());
    let output = f.add_output(1, (1920, 1080));
    inject_cache(&mut f, &tmp, &["myapp"]);

    let sid = insert_suspended(&mut f, 1, "myapp", (800, 500), (400, 300));
    let susp = StageWindow::Suspended(f.state().find_suspended(sid).unwrap());
    let eid = f.state().stage.id_of(&susp).unwrap();
    let cid = f.add_client();
    let surface = hide_under_a_stand_in_drag(&mut f, cid, sid);

    f.client(cid).window(&surface).set_fullscreen(None);
    f.double_roundtrip(cid);
    f.client(cid).window(&surface).unset_fullscreen();
    f.double_roundtrip(cid);

    f.state().disarm_interactive_move(&sid);
    f.pump(1);

    let adopted = mapped_client(&mut f, "myapp").expect("the deferred adopt landed");
    assert!(
        !f.state().is_window_fullscreen(&adopted),
        "the retracted request is not still waiting in the queue at the reveal"
    );
    assert!(
        !f.state().is_output_fullscreen(&output),
        "and nothing owns the output"
    );
    assert_eq!(
        f.state().stage.id_of(&adopted),
        Some(eid),
        "so nothing stood between the adopt and the stand-in's slot"
    );

    settle_resize(&mut f, cid, &surface, (400, 300));
    client_close(&mut f, cid, &surface);
}

/// An Alt-Tab session walks a frozen list, and the liveness sweep fires on the
/// frame tick regardless — including mid-cycle. The reveal owes the window its
/// place in that list, but not while someone is stepping through it: the write
/// waits, exactly as a focus change during a cycle does.
#[test]
fn a_reveal_mid_cycle_leaves_the_frozen_history_alone() {
    let tmp = TempDir::new();
    let mut f = Fixture::with_config(Config::default());
    f.add_output(1, (1920, 1080));
    inject_cache(&mut f, &tmp, &["myapp"]);
    origin_view(&mut f);

    let others = f.add_client();
    let a_surface = map_window(&mut f, others, "a", (200, 200));
    let b_surface = map_window(&mut f, others, "b", (200, 200));

    let sid = insert_suspended(&mut f, 1, "myapp", (5000, 5000), (400, 300));
    let cid = f.add_client();
    let surface = hide_under_a_stand_in_drag(&mut f, cid, sid);
    let hidden = mapped_client(&mut f, "myapp").unwrap();
    seat_at(&mut f, &hidden, (1000, 1000));

    // Open a held-modifier session: one step, no commit.
    let anchor = f.state().cycle_anchor();
    f.state().stage.cycle_step(false, anchor.as_ref());
    let frozen = f.state().stage.focus_history().to_vec();
    assert!(
        f.state().stage.cycle_state().is_some(),
        "precondition: a cycle is open over the list below"
    );

    f.state()
        .sweep_pending_relaunches(Instant::now() + Duration::from_secs(31));
    f.pump(1);

    assert_eq!(
        f.state()
            .element_under(Point::from((1100.0, 1050.0)))
            .map(|(w, _)| w.clone()),
        Some(hidden.clone()),
        "precondition: the sweep did reveal it — the window is on screen"
    );
    assert_eq!(
        f.state().stage.focus_history(),
        frozen.as_slice(),
        "the cycle's list is unchanged: a window appearing under it must not \
         shift what the next step lands on"
    );

    f.state().end_cycle();
    f.state().disarm_interactive_move(&sid);
    f.pump(1);
    f.state().dismiss_suspended(sid);
    client_close(&mut f, cid, &surface);
    client_close(&mut f, others, &a_surface);
    client_close(&mut f, others, &b_surface);
}

/// Two adopts can sit in the stash at once, and a release drains them one at a
/// time — here landing one and re-deferring the other, whose stand-in is under a
/// grab of its own. The one that stays hidden has to read as hidden for the whole
/// drain: the first reveal re-seats pointer focus, and a walk that finds the
/// still-invisible window routes clicks to something the user cannot see.
#[test]
fn a_reveal_does_not_unhide_the_entry_still_waiting_behind_it() {
    let tmp = TempDir::new();
    let mut f = Fixture::with_config(Config::default());
    f.add_output(1, (1920, 1080));
    inject_cache(&mut f, &tmp, &["one", "two"]);
    origin_view(&mut f);

    let released = insert_suspended(&mut f, 1, "one", (800, 500), (400, 300));
    let held = insert_suspended(&mut f, 2, "two", (5000, 5000), (400, 300));

    // Both stand-ins are being dragged. The one whose adopt lands first is
    // stashed first, so the second entry is still waiting when its reveal runs.
    f.state().relaunch_suspended(released);
    let released_token = f.state().pending_relaunch_token_for_test(released).unwrap();
    f.state().arm_interactive_move(&released);
    let released_cid = f.add_client();
    let released_surface = begin_window(&mut f, released_cid, "one");
    present_token(&mut f, released_cid, &released_surface, released_token);
    finish_window(&mut f, released_cid, &released_surface, (300, 200));

    f.state().relaunch_suspended(held);
    let held_token = f.state().pending_relaunch_token_for_test(held).unwrap();
    f.state().arm_interactive_move(&held);
    let held_cid = f.add_client();
    let held_surface = begin_window(&mut f, held_cid, "two");
    present_token(&mut f, held_cid, &held_surface, held_token);
    finish_window(&mut f, held_cid, &held_surface, (300, 200));
    let held_window = mapped_client(&mut f, "two").unwrap();
    seat_at(&mut f, &held_window, (1400, 100));
    assert_eq!(
        f.state().debug_counters()["deferred_adoptions"],
        2,
        "precondition: both adopts are stashed, so both windows are hidden"
    );

    // The pointer rests over the window that will still be hidden afterwards.
    motion(&mut f, Point::from((1500.0, 150.0)));

    f.state().disarm_interactive_move(&released);
    f.pump(1);

    let adopted = mapped_client(&mut f, "one").expect("the released adopt landed");
    assert_eq!(
        f.state().stage.position_of(&adopted),
        Some(Point::from((800, 500))),
        "precondition: the release drained its own entry into the stand-in's slot"
    );
    assert_eq!(
        f.state().debug_counters()["deferred_adoptions"],
        1,
        "precondition: the other entry re-deferred under the grab still holding it"
    );
    assert!(
        f.state()
            .seat
            .get_pointer()
            .unwrap()
            .current_focus()
            .is_none(),
        "the pointer found nothing where the still-hidden window sits, instead \
         of being handed to a window nothing is drawn for"
    );

    settle_resize(&mut f, released_cid, &released_surface, (400, 300));
    client_close(&mut f, released_cid, &released_surface);
    f.state().disarm_interactive_move(&held);
    f.pump(1);
    f.state().dismiss_suspended(held);
    client_close(&mut f, held_cid, &held_surface);
}

/// `msg suspend` reaches a window still hidden for a deferred adopt through its
/// selector alone: it suspends the window it resolved rather than the one the
/// keyboard is on, which is what lets it name one the focus primitives refuse.
/// The mark it leaves carries its own rect and an explicit "suspend this", so
/// the conversion the hiding refuses on its own account has to go through —
/// otherwise the command silently degrades into a plain close and the app is
/// gone with nothing to bring it back.
#[test]
fn ipc_suspend_of_a_hidden_adopt_still_leaves_a_stand_in() {
    use crate::ipc::protocol::{Request, Response, WindowSelector};

    let tmp = TempDir::new();
    let mut f = Fixture::with_config(Config::default());
    f.add_output(1, (1920, 1080));
    inject_cache(&mut f, &tmp, &["myapp"]);
    origin_view(&mut f);

    let sid = insert_suspended(&mut f, 1, "myapp", (800, 500), (400, 300));
    let cid = f.add_client();
    let surface = hide_under_a_stand_in_drag(&mut f, cid, sid);
    let hidden = mapped_client(&mut f, "myapp").unwrap();

    let win_id = f
        .state()
        .stage
        .id_of(&StageWindow::Client(hidden))
        .unwrap()
        .0;
    let reply = crate::ipc::dispatch(
        Request::Suspend(Some(WindowSelector::Id(win_id))),
        f.state(),
    );
    assert!(matches!(reply, Ok(Response::Ok)));

    client_close(&mut f, cid, &surface);

    let stand_ins: Vec<SuspendedId> = f
        .state()
        .stage
        .windows()
        .filter_map(|w| w.suspended().map(|s| s.id))
        .collect();
    assert_eq!(
        stand_ins.len(),
        2,
        "the explicit suspend left its own stand-in beside the one being dragged"
    );

    f.state().disarm_interactive_move(&sid);
    for id in stand_ins {
        f.state().dismiss_suspended(id);
    }
}

/// A toolkit that mints its own activation token when it presents its first
/// window reaches the activation path with that window still hidden, and the
/// tail of that path raises it, hands it the keyboard and flies the camera to
/// the holding placement the flush is about to teleport it out of — dropping the
/// output out of fullscreen on the way, to make room for a window nobody can
/// see. The identical request, once the window is revealed, is honored in full.
#[test]
fn an_activation_for_a_hidden_adopt_moves_neither_the_camera_nor_the_keyboard() {
    let tmp = TempDir::new();
    let mut f = Fixture::with_config(Config::default());
    let output = f.add_output(1, (1920, 1080));
    inject_cache(&mut f, &tmp, &["myapp"]);
    // The honored activation below pans, which populates blur_camera_generation.
    f.skip_baseline_check();

    let fs = f.add_client();
    let fs_surface = map_window(&mut f, fs, "fs", (400, 300));
    let fs_window = window_by_app_id(&mut f, "fs").unwrap();
    f.client(fs).window(&fs_surface).set_fullscreen(None);
    f.double_roundtrip(fs);
    adopt_last_configure(&mut f, fs, &fs_surface);

    let sid = insert_suspended(&mut f, 1, "myapp", (5000, 5000), (400, 300));
    let cid = f.add_client();
    let surface = hide_under_a_stand_in_drag(&mut f, cid, sid);
    let hidden = mapped_client(&mut f, "myapp").unwrap();
    // Far off screen, so a flight to it is unmistakable.
    seat_at(&mut f, &hidden, (9000, 9000));
    f.state().set_camera_target(None);

    // The app's own token, carrying a serial, presented on its own window.
    let self_activate = |f: &mut Fixture| {
        f.client(cid).request_activation_token(&surface, true);
        f.roundtrip(cid);
        f.client(cid).activate(&surface);
        f.double_roundtrip(cid);
    };
    self_activate(&mut f);

    assert_eq!(
        f.state().focused_window(),
        Some(fs_window.clone()),
        "the keyboard stayed with the window the user is looking at"
    );
    assert!(
        f.state().camera_target().is_none(),
        "and no flight was aimed at the holding placement"
    );
    assert!(
        f.state().is_output_fullscreen(&output),
        "and the screen was not cleared to make room for a window nobody can see"
    );

    // Revealed where it stands (the stand-in is gone, so nothing teleports it),
    // the same request is the ordinary activation it always was.
    release_onto_a_dismissed_stand_in(&mut f, sid);
    self_activate(&mut f);

    assert_eq!(
        f.state().focused_window(),
        Some(hidden.clone()),
        "the refusal above was the hiding, not a request that never arrived"
    );
    assert!(
        f.state().camera_target().is_some(),
        "and the flight it was owed lands once there is something to fly to"
    );
    assert!(
        !f.state().is_output_fullscreen(&output),
        "including the fullscreen exit the activation makes room with"
    );

    client_close(&mut f, cid, &surface);
    client_close(&mut f, fs, &fs_surface);
}

/// The refusal belongs to the primitives, not to the routes into them: whatever
/// resolved a hidden window — an IPC selector, a taskbar, a follow target — must
/// not be able to raise it, seat the keyboard on it, or aim a camera at it.
#[test]
fn the_focus_and_camera_primitives_refuse_a_hidden_adopt() {
    use crate::state::FocusTarget;
    use smithay::utils::SERIAL_COUNTER;

    let tmp = TempDir::new();
    let mut f = Fixture::with_config(Config::default());
    f.add_output(1, (1920, 1080));
    inject_cache(&mut f, &tmp, &["myapp"]);
    origin_view(&mut f);
    f.skip_baseline_check();

    let sid = insert_suspended(&mut f, 1, "myapp", (5000, 5000), (400, 300));
    let other = f.add_client();
    let other_surface = map_window(&mut f, other, "other", (400, 300));
    let other_win = window_by_app_id(&mut f, "other").unwrap();

    let cid = f.add_client();
    let surface = hide_under_a_stand_in_drag(&mut f, cid, sid);
    let hidden = mapped_client(&mut f, "myapp").unwrap();
    seat_at(&mut f, &hidden, (9000, 9000));
    // Put the visible window on top and in the keyboard, so a raise or a focus
    // that did land would show.
    f.state()
        .raise_and_focus(&other_win, SERIAL_COUNTER.next_serial());
    f.state().set_camera_target(None);

    let topmost = |f: &mut Fixture| f.state().stage.windows().next_back().cloned();
    let on_top = StageWindow::Client(other_win.clone());

    f.state().navigate_to_window(&hidden, true);
    assert!(
        f.state().camera_target().is_none(),
        "no camera flight to a window nothing is drawn for"
    );

    f.state()
        .raise_and_focus(&hidden, SERIAL_COUNTER.next_serial());
    assert_eq!(
        topmost(&mut f),
        Some(on_top.clone()),
        "and no raise into a z-slot the adopt is about to replace outright"
    );

    f.state().set_window_focus(
        Some(FocusTarget(server_surface(&hidden))),
        SERIAL_COUNTER.next_serial(),
    );
    assert_eq!(
        f.state().focused_window(),
        Some(other_win.clone()),
        "and the keyboard stays where the user left it"
    );

    // Revealed, the very same three calls do what they say — so the assertions
    // above are about the hiding, not about calls that never do anything. The
    // reveal raises and focuses the window itself, so put the visible one back
    // on top and in the keyboard first: otherwise all three would read as
    // honored even if the primitives went on refusing.
    release_onto_a_dismissed_stand_in(&mut f, sid);
    f.state()
        .raise_and_focus(&other_win, SERIAL_COUNTER.next_serial());
    f.state().set_camera_target(None);
    f.state().navigate_to_window(&hidden, true);
    assert!(f.state().camera_target().is_some());
    assert_eq!(topmost(&mut f), Some(StageWindow::Client(hidden.clone())));
    assert_eq!(f.state().focused_window(), Some(hidden.clone()));

    client_close(&mut f, cid, &surface);
    client_close(&mut f, other, &other_surface);
}

/// A client that restores maximized at startup asks while it is hidden, so the
/// request waits for the flush. Handed over at the reveal — a step ahead of the
/// adopt — the fit frames the holding placement the window is about to be
/// teleported out of, and leaves the camera on ground the adopt then moves it
/// off. It has to measure the slot the window actually lands in.
#[test]
fn a_queued_fit_frames_the_slot_the_adopt_lands_in() {
    use smithay::reexports::wayland_server::Resource;

    let tmp = TempDir::new();
    let mut f = Fixture::with_config(Config::default());
    f.add_output(1, (1920, 1080));
    inject_cache(&mut f, &tmp, &["myapp"]);
    origin_view(&mut f);
    f.skip_baseline_check();

    // The slot is far from the holding placement, which lands near the camera.
    let sid = insert_suspended(&mut f, 1, "myapp", (5000, 5000), (400, 300));
    let cid = f.add_client();
    let surface = hide_under_a_stand_in_drag(&mut f, cid, sid);
    let hidden = mapped_client(&mut f, "myapp").unwrap();

    f.client(cid).window(&surface).set_maximized();
    f.double_roundtrip(cid);

    // Inside the rect the fit is about to hand the window, outside both the
    // holding placement and the slot the adopt drops it in.
    motion(&mut f, Point::from((4400.0, 4700.0)));

    f.state().disarm_interactive_move(&sid);
    f.pump(1);

    assert!(
        !suspended_present(&mut f),
        "precondition: the adopt took the stand-in's slot"
    );
    assert_eq!(
        f.state()
            .seat
            .get_pointer()
            .unwrap()
            .current_focus()
            .map(|t| t.0),
        Some(server_surface(&hidden)),
        "the pointer found it there — the reveal's pass and the adopt's both \
         ran before the fit moved the window, under a pointer that has not \
         moved since"
    );
    let target = f.state().camera_target();
    assert!(
        target.is_some_and(|t| t.x > 3000.0 && t.y > 3000.0),
        "the fit framed the slot the window landed in — a fit taken ahead of \
         the adopt frames the holding placement instead, and then loses even \
         that when the adopt drops the animation its pan was parked on: \
         {target:?}"
    );
    assert!(
        f.state().stage.is_fit(&hidden),
        "and the membership survives, which a fit taken before the adopt does \
         not — the stage surgery drops it, leaving the client told it is \
         maximized while nothing here agrees"
    );

    // The adopt owes a stable snap rect, payable on a commit at the size it
    // configured — a size this fit means the client never to be asked for. A fit
    // writes no stable rect of its own either, so the debt has to be settled
    // against the adopted slot on the way in or the window carries no settled
    // footprint at all: its close degrades to a cluster of one, and shrink
    // protection stays off until some later grab writes one.
    let root_id = server_surface(&hidden).id();
    let stable = f.state().stable_snap_rects.get(&root_id).copied();
    assert!(
        stable.is_some_and(
            |r| (r.x_low, r.x_high, r.y_low, r.y_high) == (5000.0, 5400.0, 5000.0, 5300.0)
        ),
        "the settled footprint is the adopted slot, not the fit rect and not \
         nothing: {stable:?}"
    );
    assert!(
        !f.state().pending_adopt_settle.contains_key(&root_id),
        "and the debt is paid rather than left owing for the window's lifetime"
    );

    client_close(&mut f, cid, &surface);
}

/// A dialog the relaunching app opened before its window was revealed can die
/// inside the deferral, and a close's follow tiers resolve its parent through
/// the xdg link — a window that reads as off screen, cannot be focused and
/// cannot be panned to. Kept as the target it suppresses the tiers that would
/// have found a real one, and the close raises and focuses nothing at all.
#[test]
fn a_close_does_not_follow_into_a_hidden_adopt() {
    let tmp = TempDir::new();
    let mut f = Fixture::with_config(Config::default());
    f.add_output(1, (1920, 1080));
    inject_cache(&mut f, &tmp, &["myapp"]);
    origin_view(&mut f);

    let sid = insert_suspended(&mut f, 1, "myapp", (5000, 5000), (400, 300));

    let other = f.add_client();
    let other_surface = map_window(&mut f, other, "other", (400, 300));
    let other_win = window_by_app_id(&mut f, "other").unwrap();

    let cid = f.add_client();
    let surface = hide_under_a_stand_in_drag(&mut f, cid, sid);

    // The app's own dialog, parented to the window nobody can see yet.
    let dialog = begin_window(&mut f, cid, "myapp-dialog");
    let parent = f.client(cid).window(&surface).xdg_toplevel.clone();
    f.client(cid).window(&dialog).set_parent(Some(&parent));
    finish_window(&mut f, cid, &dialog, (200, 100));
    let dialog_win = window_by_app_id(&mut f, "myapp-dialog").unwrap();
    assert_eq!(
        f.state().focused_window(),
        Some(dialog_win),
        "precondition: the dialog holds the keyboard, so its close follows"
    );

    client_close(&mut f, cid, &dialog);

    assert_eq!(
        f.state().focused_element(),
        Some(StageWindow::Client(other_win.clone())),
        "the close skipped its invisible parent and landed on a window the \
         user can see"
    );
    assert_eq!(
        f.state().stage.windows().next_back().cloned(),
        Some(StageWindow::Client(other_win)),
        "and raised it — a follow into the hidden parent raises nothing, \
         leaving the focus to drift back through the history on its own"
    );

    f.state().disarm_interactive_move(&sid);
    f.pump(1);
    client_close(&mut f, cid, &surface);
    client_close(&mut f, other, &other_surface);
}

/// `msg focus` can name a window still hidden for a deferred adopt, and the
/// keyboard may not land there. It refuses rather than reporting a focus it did
/// not make — the window arrives on its own when the grab lets go.
#[test]
fn ipc_focus_of_a_hidden_adopt_is_refused() {
    use crate::ipc::protocol::{Request, WindowSelector};

    let tmp = TempDir::new();
    let mut f = Fixture::with_config(Config::default());
    f.add_output(1, (1920, 1080));
    inject_cache(&mut f, &tmp, &["myapp"]);
    origin_view(&mut f);

    let sid = insert_suspended(&mut f, 1, "myapp", (5000, 5000), (400, 300));
    let other = f.add_client();
    let other_surface = map_window(&mut f, other, "other", (400, 300));
    let other_win = window_by_app_id(&mut f, "other").unwrap();

    let cid = f.add_client();
    let surface = hide_under_a_stand_in_drag(&mut f, cid, sid);
    let hidden = mapped_client(&mut f, "myapp").unwrap();
    let win_id = f
        .state()
        .stage
        .id_of(&StageWindow::Client(hidden))
        .unwrap()
        .0;

    let reply = crate::ipc::dispatch(Request::Focus(Some(WindowSelector::Id(win_id))), f.state());
    assert!(reply.is_err(), "got {reply:?}");
    assert_eq!(
        f.state().focused_window(),
        Some(other_win),
        "and the keyboard stayed where it was"
    );

    // Nor is the id one a bar could have offered in the first place: the app is
    // already in the inventory as the stand-in the window is bound for, and a
    // second entry at a placement nothing is drawn at is one whose only reply is
    // the refusal above.
    let listed: Vec<bool> = f
        .state()
        .window_inventory()
        .iter()
        .filter(|w| w.app_id == "myapp")
        .map(|w| w.suspended)
        .collect();
    assert_eq!(
        listed,
        vec![true],
        "the app is listed once, as the stand-in — not twice, the second at a \
         placement nothing is drawn at"
    );

    // The selector resolves fine once the window is on screen, so the refusal
    // was the hiding rather than a window the command could not find.
    release_onto_a_dismissed_stand_in(&mut f, sid);
    let reply = crate::ipc::dispatch(Request::Focus(Some(WindowSelector::Id(win_id))), f.state());
    assert!(reply.is_ok(), "got {reply:?}");

    client_close(&mut f, cid, &surface);
    client_close(&mut f, other, &other_surface);
}

/// The stash outlives the grab that filled it: a release only schedules the
/// flush, and a client request dispatched in the same round — a re-presented
/// relaunch token, or the commit of a rule-forced size — can reach the adopt
/// first. Whichever gets there, the adopt is the end of the hiding: its own
/// refocus has to land, or the keyboard is left aimed at a stand-in the
/// `replace` has just consumed, and the window is only rescued by a later reveal
/// that raises it back out of the z-slot the adopt exists to inherit.
#[test]
fn an_adopt_that_beats_the_flush_ends_the_hiding_itself() {
    use crate::state::AdoptOrigin;
    use smithay::utils::SERIAL_COUNTER;

    let tmp = TempDir::new();
    let mut f = Fixture::with_config(Config::default());
    f.add_output(1, (1920, 1080));
    inject_cache(&mut f, &tmp, &["myapp"]);
    origin_view(&mut f);

    // Mapped after the stand-in, so it sits above the slot the adopt inherits
    // and a raise nobody asked for shows.
    let sid = insert_suspended(&mut f, 1, "myapp", (5000, 5000), (400, 300));
    let other = f.add_client();
    let other_surface = map_window(&mut f, other, "other", (400, 300));
    let other_win = window_by_app_id(&mut f, "other").unwrap();
    let on_top = StageWindow::Client(other_win.clone());
    // Hovered rather than clicked: the stand-in holds the focus the adopt
    // inherits without being raised over the window above it.
    f.state()
        .set_suspended_focus(sid, SERIAL_COUNTER.next_serial());

    let cid = f.add_client();
    let surface = hide_under_a_stand_in_drag(&mut f, cid, sid);
    let hidden = mapped_client(&mut f, "myapp").unwrap();

    // The button is up, so nothing fights the adopt any more — but the flush is
    // still queued behind this dispatch rather than run.
    f.state().disarm_interactive_move(&sid);
    let root = server_surface(&hidden);
    f.state()
        .resolve_placed_adopt(&hidden, &root, sid, AdoptOrigin::Activation);

    assert_eq!(
        f.state().debug_counters()["deferred_adoptions"],
        0,
        "the adopt dropped the entry that was hiding its own window"
    );
    assert_eq!(
        f.state().stage.position_of(&hidden),
        Some(Point::from((5000, 5000))),
        "precondition: it took the stand-in's slot"
    );
    assert_eq!(
        f.state().focused_window(),
        Some(hidden.clone()),
        "and its refocus landed, rather than being refused and leaving the \
         intent on a stand-in that no longer exists"
    );

    f.pump(1);

    assert_eq!(
        f.state().stage.windows().next_back().cloned(),
        Some(on_top),
        "the flush found nothing left to reveal — a reveal here raises the \
         window back out of the slot it just inherited"
    );

    settle_resize(&mut f, cid, &surface, (400, 300));
    client_close(&mut f, cid, &surface);
    client_close(&mut f, other, &other_surface);
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
