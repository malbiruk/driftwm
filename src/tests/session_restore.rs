//! Durable session store + restore: the quit-serialize round-trip, origin
//! filtering with carry-forward when `restore_windows` is off, fresh-boot camera
//! seeding, and the immediate write on create/dismiss. The fixture drives the
//! same `serialize_session_on_shutdown` the main.rs choke point calls; the
//! post-`run()` wiring itself (Quit + signalfd both reaching it) is hardware
//! smoke, not covered here.

use std::collections::BTreeMap;
use std::rc::Rc;

use driftwm::config::Config;
use driftwm::desktop_entry::DesktopEntryCache;
use driftwm::session::{self, Origin, SessionEntry, SessionEnvelope, SessionOutput};
use smithay::utils::{Point, Rectangle, Size};

use crate::decorations::DecorationHit;
use crate::input::DecoTarget;
use crate::state::{StageWindow, SuspendedWindow};

use super::real::TempDir;
use super::{Fixture, map_window, window_by_app_id};

/// SSD-on config with `[session].restore_windows` set as asked.
fn config_restore(on: bool) -> Config {
    Config::from_toml(&format!(
        "[session]\nrestore_windows = {on}\n[decorations]\ndefault_mode = \"server\"\n"
    ))
    .unwrap()
}

/// Seat a desktop-entry cache resolving each `stem` to a launchable identity.
fn inject_cache(f: &mut Fixture, tmp: &TempDir, stems: &[&str]) {
    for stem in stems {
        let contents = format!("[Desktop Entry]\nType=Application\nName={stem}\nExec={stem}\n");
        std::fs::write(tmp.path().join(format!("{stem}.desktop")), contents).unwrap();
    }
    f.state().desktop_entry_cache = Some(DesktopEntryCache::new(vec![tmp.path().to_path_buf()]));
}

/// Map a client at `app_id`/`size` parked at a known canvas position.
fn map_at(
    f: &mut Fixture,
    id: super::client::ClientId,
    app_id: &str,
    size: (u16, u16),
    pos: (i32, i32),
) {
    map_window(f, id, app_id, size);
    let window = window_by_app_id(f, app_id).unwrap();
    f.state()
        .map_window(StageWindow::Client(window), Point::from(pos), true);
}

/// The suspended stand-ins on the stage, in z-order (bottom→top), each with its
/// canvas position.
fn suspended_in_order(
    f: &mut Fixture,
) -> Vec<(Rc<SuspendedWindow>, Point<i32, smithay::utils::Logical>)> {
    let stage = &f.state().stage;
    stage
        .windows()
        .filter_map(|w| {
            let s = w.suspended()?;
            let pos = stage.position_of(w).unwrap_or_default();
            Some((s.clone(), pos))
        })
        .collect()
}

fn entry(id: u64, app: &str, origin: Origin) -> SessionEntry {
    SessionEntry {
        id,
        app_id: app.to_string(),
        desktop_id: format!("{app}.desktop"),
        display_name: app.to_uppercase(),
        title: format!("{app}-title"),
        position: [100, 200],
        size: [400, 300],
        origin,
        csd: false,
    }
}

/// Serialize live windows on quit (`restore_windows = true`), then a fresh
/// `DriftWm` materializes them in z-order at their exact rects with `Quit`
/// origin. Drives the factored serialize fn the choke point calls.
#[test]
fn quit_serialize_round_trip() {
    let tmp = TempDir::new();
    let path = tmp.path().join("session.json");

    // A prior session with two windows, bottom→top: alpha then beta.
    {
        let cache = TempDir::new();
        let mut f = Fixture::with_config(config_restore(true));
        f.add_output(1, (1920, 1080));
        inject_cache(&mut f, &cache, &["alpha", "beta"]);
        f.state().session_store.path = Some(path.clone());

        let a = f.add_client();
        map_at(&mut f, a, "alpha", (400, 300), (500, 500));
        let b = f.add_client();
        map_at(&mut f, b, "beta", (200, 200), (-300, 100));

        f.state().serialize_session_on_shutdown();
    }

    // The file holds both, in z-order, as quit records.
    let saved = session::read(&path);
    assert_eq!(saved.entries.len(), 2);
    assert_eq!(saved.entries[0].app_id, "alpha");
    assert_eq!(saved.entries[1].app_id, "beta");
    assert!(saved.entries.iter().all(|e| e.origin == Origin::Quit));

    // A fresh compositor materializes them in order. These are CSD windows, so
    // their stand-in bodies are shrunk under the bar (footprint preserved): the
    // Quit record persists the shrunken body, restore lands at it verbatim.
    let mut f = Fixture::with_config(config_restore(true));
    f.add_output(1, (1920, 1080));
    f.state().session_store.path = Some(path.clone());
    f.state().load_session();

    let restored = suspended_in_order(&mut f);
    assert_eq!(restored.len(), 2);
    assert_eq!(restored[0].0.identity.app_id, "alpha");
    assert_eq!(restored[0].1, Point::from((500, 525)));
    assert_eq!(restored[0].0.size.get(), Size::from((400, 275)));
    assert_eq!(restored[0].0.origin, Origin::Quit);
    assert_eq!(restored[1].0.identity.app_id, "beta");
    assert_eq!(restored[1].1, Point::from((-300, 125)));
    assert_eq!(restored[1].0.size.get(), Size::from((200, 175)));

    for (s, _) in restored {
        f.state().dismiss_suspended(s.id);
    }
}

/// A restored stand-in renders the same centered clickable name as a
/// conversion-born one: its display name survives the round-trip, and the
/// label cache tracks font-readiness so a label built before the startup font
/// scan lands re-rasters with text once it does.
#[test]
fn restored_stand_in_has_clickable_label() {
    let tmp = TempDir::new();
    let path = tmp.path().join("session.json");
    let envelope = SessionEnvelope {
        version: session::VERSION,
        bookmarks: BTreeMap::new(),
        saved_at: 0,
        entries: vec![entry(1, "myapp", Origin::Explicit)],
        outputs: BTreeMap::new(),
    };
    session::write(&path, &envelope, false).unwrap();

    let mut f = Fixture::with_config(config_restore(true));
    f.add_output(1, (1920, 1080));
    f.state().session_store.path = Some(path.clone());
    f.state().load_session();

    let restored = suspended_in_order(&mut f);
    assert_eq!(restored.len(), 1);
    let (s, pos) = (restored[0].0.clone(), restored[0].1);
    let sid = s.id;
    // The label's text source survived restore.
    assert!(
        !s.identity.display_name.is_empty(),
        "restored display name is non-empty"
    );

    // Build the label as the render pass does, with the font scan not yet
    // landed: the key records fonts_ready = false.
    let cold = f
        .state()
        .build_suspended_chrome_for_test(sid, false, false)
        .unwrap();
    assert!(!cold.4, "cold key marks fonts-not-ready");
    // Once the scan lands, the same size/scale re-rasters — a different key
    // means the empty cold label is rebuilt, not kept forever.
    let warm = f
        .state()
        .build_suspended_chrome_for_test(sid, false, true)
        .unwrap();
    assert!(warm.4, "warm key marks fonts-ready");
    assert_ne!(cold, warm, "font readiness invalidates the label cache");

    // With a rendered label present (simulated — the headless fixture rasters no
    // text), the restored stand-in's body center is a Label (relaunch) hit.
    s.chrome.borrow_mut().label_rect = Some(Rectangle::new(
        Point::from((150, 130)),
        Size::from((100, 40)),
    ));
    let body_center = Point::from((pos.x as f64 + 200.0, pos.y as f64 + 150.0));
    assert!(
        matches!(
            f.state().decoration_under(body_center),
            Some((DecoTarget::Suspended(_), DecorationHit::Label))
        ),
        "the restored stand-in's centered name is clickable"
    );

    f.state().dismiss_suspended(sid);
}

/// With `restore_windows` off, an explicit entry materializes but a quit entry
/// does not — and the quit entry is carried forward on the next rewrite, so a
/// flag-off session never destroys the saved session.
#[test]
fn flag_off_materializes_explicit_and_carries_quit() {
    let tmp = TempDir::new();
    let path = tmp.path().join("session.json");

    // A prior session saved one explicit + one quit entry.
    let envelope = SessionEnvelope {
        version: session::VERSION,
        bookmarks: BTreeMap::new(),
        saved_at: 0,
        entries: vec![
            entry(1, "keepme", Origin::Explicit),
            entry(2, "onlyquit", Origin::Quit),
        ],
        outputs: BTreeMap::new(),
    };
    session::write(&path, &envelope, false).unwrap();

    let mut f = Fixture::with_config(config_restore(false));
    f.add_output(1, (1920, 1080));
    f.state().session_store.path = Some(path.clone());
    f.state().load_session();

    // Only the explicit entry is on the canvas.
    let restored = suspended_in_order(&mut f);
    assert_eq!(restored.len(), 1);
    assert_eq!(restored[0].0.identity.app_id, "keepme");

    // Dismissing it rewrites the file — the carried quit entry survives.
    f.state().dismiss_suspended(restored[0].0.id);
    let after = session::read(&path);
    assert_eq!(after.entries.len(), 1);
    assert_eq!(after.entries[0].app_id, "onlyquit");
    assert_eq!(after.entries[0].origin, Origin::Quit);
}

/// Restore flipped on after a flag-off boot must not duplicate a relaunched app:
/// the carried-forward quit entry is dropped at shutdown (the live canvas is
/// authoritative), so the app serializes once, not twice.
#[test]
fn restore_flip_on_drops_carried_quit_for_relaunched_app() {
    let cache = TempDir::new();
    let tmp = TempDir::new();
    let path = tmp.path().join("session.json");

    // A prior session left a quit entry for "onlyquit".
    let envelope = SessionEnvelope {
        version: session::VERSION,
        bookmarks: BTreeMap::new(),
        saved_at: 0,
        entries: vec![entry(2, "onlyquit", Origin::Quit)],
        outputs: BTreeMap::new(),
    };
    session::write(&path, &envelope, false).unwrap();

    // Boot with restore off: the quit entry is carried, not materialized.
    let mut f = Fixture::with_config(config_restore(false));
    f.add_output(1, (1920, 1080));
    inject_cache(&mut f, &cache, &["onlyquit"]);
    f.state().session_store.path = Some(path.clone());
    f.state().load_session();
    assert_eq!(
        suspended_in_order(&mut f).len(),
        0,
        "nothing materializes while restore is off"
    );

    // The user relaunches the app — now a live window on the canvas.
    let id = f.add_client();
    map_at(&mut f, id, "onlyquit", (400, 300), (300, 300));

    // Config hot-reload flips restore on; shutdown serializes the live windows.
    f.state().config.session.restore_windows = true;
    f.state().serialize_session_on_shutdown();

    // The app is written exactly once (the live window), not duplicated by the
    // carried-forward quit entry.
    let after = session::read(&path);
    let count = after
        .entries
        .iter()
        .filter(|e| e.app_id == "onlyquit")
        .count();
    assert_eq!(
        count, 1,
        "the relaunched app serializes once, with no carried duplicate"
    );
}

/// Count-matched dedup: flipping restore on drops a carried quit record only for
/// an app that actually came back. An app carried but not relaunched survives to
/// the next boot, unaffected by the flag flip.
#[test]
fn restore_flip_on_preserves_unrelaunched_carried_quit() {
    let cache = TempDir::new();
    let tmp = TempDir::new();
    let path = tmp.path().join("session.json");

    // A prior session left quit entries for two apps, A and B.
    let envelope = SessionEnvelope {
        version: session::VERSION,
        bookmarks: BTreeMap::new(),
        saved_at: 0,
        entries: vec![
            entry(1, "appa", Origin::Quit),
            entry(2, "appb", Origin::Quit),
        ],
        outputs: BTreeMap::new(),
    };
    session::write(&path, &envelope, false).unwrap();

    // Boot with restore off: both quit entries carry, neither materializes.
    let mut f = Fixture::with_config(config_restore(false));
    f.add_output(1, (1920, 1080));
    inject_cache(&mut f, &cache, &["appa", "appb"]);
    f.state().session_store.path = Some(path.clone());
    f.state().load_session();

    // The user relaunches only A.
    let id = f.add_client();
    map_at(&mut f, id, "appa", (400, 300), (300, 300));

    // Flip restore on, then quit.
    f.state().config.session.restore_windows = true;
    f.state().serialize_session_on_shutdown();

    let after = session::read(&path);
    // A's carried quit was deduped against the live window — a single entry.
    assert_eq!(
        after.entries.iter().filter(|e| e.app_id == "appa").count(),
        1,
        "the relaunched app is serialized once"
    );
    // B never came back, so its carried quit survives to the next boot.
    assert!(
        after
            .entries
            .iter()
            .any(|e| e.app_id == "appb" && e.origin == Origin::Quit),
        "the un-relaunched carried quit entry is preserved, not destroyed"
    );
}

/// With `[session].restore_camera` on, a durable per-output camera seeds a
/// freshly connected output on fresh boot (no runtime entry). Runtime-wins is
/// exercised by the `merge_saved_cameras` unit test.
#[test]
fn durable_camera_seeds_fresh_boot() {
    let tmp = TempDir::new();
    let path = tmp.path().join("session.json");

    let mut outputs = BTreeMap::new();
    outputs.insert(
        "HEADLESS-1".to_string(),
        SessionOutput {
            camera: [-1234.0, -5678.0],
            // A real zoom-out value: the compositor caps zoom at MAX_ZOOM (1.0),
            // and out-of-bounds seeds are rejected on load.
            zoom: 0.75,
        },
    );
    let envelope = SessionEnvelope {
        version: session::VERSION,
        bookmarks: BTreeMap::new(),
        saved_at: 0,
        entries: Vec::new(),
        outputs,
    };
    session::write(&path, &envelope, false).unwrap();

    let mut f = Fixture::with_config(config_restore(false));
    f.state().config.session.restore_camera = true;
    f.state().session_store.path = Some(path.clone());
    f.state().load_session();

    // Fresh boot: no runtime entry for HEADLESS-1, so the durable seed applies.
    let seed = f.state().session_store.durable_cameras.clone();
    let (output, _global) =
        super::headless::add_output_with_saved(f.state(), 1, (1920, 1080), &seed);
    let (camera, zoom) = {
        let os = crate::state::output_state(&output);
        (os.camera, os.zoom)
    };
    assert_eq!(camera, Point::from((-1234.0, -5678.0)));
    assert_eq!(zoom, 0.75);
}

/// A parseable entry with out-of-range geometry (a hand-edit / flipped byte)
/// is dropped at load — never materialized (no `Size::from` panic) and never
/// carried forward, so it's gone from the next serialize.
#[test]
fn out_of_range_entry_is_dropped_and_not_carried() {
    let tmp = TempDir::new();
    let path = tmp.path().join("session.json");

    let mut bad = entry(1, "bad", Origin::Explicit);
    bad.size = [-1, 300];
    let good = entry(2, "good", Origin::Explicit);
    let envelope = SessionEnvelope {
        version: session::VERSION,
        bookmarks: BTreeMap::new(),
        saved_at: 0,
        entries: vec![bad, good],
        outputs: BTreeMap::new(),
    };
    session::write(&path, &envelope, false).unwrap();

    let mut f = Fixture::with_config(config_restore(true));
    f.add_output(1, (1920, 1080));
    f.state().session_store.path = Some(path.clone());
    // No panic on load; only the valid entry materializes.
    f.state().load_session();

    let restored = suspended_in_order(&mut f);
    assert_eq!(restored.len(), 1, "the negative-size entry was dropped");
    assert_eq!(restored[0].0.identity.app_id, "good");

    // The bad entry is gone from the next serialize too (not carried forward).
    f.state().serialize_session_on_shutdown();
    let after = session::read(&path);
    assert!(
        after.entries.iter().all(|e| e.app_id != "bad"),
        "the dropped entry does not reappear"
    );
    for (s, _) in restored {
        f.state().dismiss_suspended(s.id);
    }
}

/// A `zoom: 0.0` durable seed (hand-edit / corruption) is filtered at load, so
/// the output connects with its default camera/zoom — no inf/NaN viewport — and
/// the next serialize writes the live sane value, self-healing across restarts.
#[test]
fn invalid_zoom_seed_is_ignored_and_reserializes_sane() {
    let tmp = TempDir::new();
    let path = tmp.path().join("session.json");

    let mut outputs = BTreeMap::new();
    outputs.insert(
        "HEADLESS-1".to_string(),
        SessionOutput {
            camera: [-960.0, -540.0],
            zoom: 0.0,
        },
    );
    let envelope = SessionEnvelope {
        version: session::VERSION,
        bookmarks: BTreeMap::new(),
        saved_at: 0,
        entries: Vec::new(),
        outputs,
    };
    session::write(&path, &envelope, false).unwrap();

    let mut f = Fixture::with_config(config_restore(false));
    f.state().config.session.restore_camera = true;
    f.state().session_store.path = Some(path.clone());
    f.state().load_session();

    // The invalid seed was dropped from the durable cameras entirely.
    assert!(
        !f.state()
            .session_store
            .durable_cameras
            .contains_key("HEADLESS-1"),
        "zoom 0.0 seed filtered out"
    );

    // The output connects with the default centered camera/zoom.
    let seed = f.state().session_store.durable_cameras.clone();
    let (output, _global) =
        super::headless::add_output_with_saved(f.state(), 1, (1920, 1080), &seed);
    let (camera, zoom) = {
        let os = crate::state::output_state(&output);
        (os.camera, os.zoom)
    };
    assert_eq!(zoom, 1.0, "default zoom, not 0.0");
    assert_eq!(camera, Point::from((-960.0, -540.0)));

    // The next serialize records the live sane zoom, not the corrupt 0.0.
    f.state().session_store.path = Some(path.clone());
    f.state().serialize_session_on_shutdown();
    let after = session::read(&path);
    assert_eq!(
        after.outputs.get("HEADLESS-1").map(|o| o.zoom),
        Some(1.0),
        "the corrupt zoom self-healed on the next write"
    );
}

/// With `restore_camera` off (the default), a durable per-output camera is not
/// seeded — the output connects at its default centered camera — while saved
/// windows still materialize.
#[test]
fn restore_camera_off_skips_seed_but_materializes_windows() {
    let tmp = TempDir::new();
    let path = tmp.path().join("session.json");

    let mut outputs = BTreeMap::new();
    outputs.insert(
        "HEADLESS-1".to_string(),
        SessionOutput {
            camera: [-1234.0, -5678.0],
            zoom: 0.75,
        },
    );
    let envelope = SessionEnvelope {
        version: session::VERSION,
        bookmarks: BTreeMap::new(),
        saved_at: 0,
        // An Explicit entry materializes regardless of restore_windows, so this
        // isolates the camera flag.
        entries: vec![entry(1, "good", Origin::Explicit)],
        outputs,
    };
    session::write(&path, &envelope, false).unwrap();

    // Default config: restore_camera is off.
    let mut f = Fixture::with_config(Config::default());
    assert!(
        !f.state().config.session.restore_camera,
        "restore_camera defaults off"
    );
    f.state().session_store.path = Some(path.clone());
    f.state().load_session();

    // The durable camera is still stashed (so the write side can carry it
    // forward), but withheld from a connecting output while the flag is off.
    assert!(
        f.state()
            .session_store
            .durable_cameras
            .contains_key("HEADLESS-1"),
        "the durable camera is carried for the write side even with restore off"
    );
    assert!(
        !f.state().saved_camera_state().contains_key("HEADLESS-1"),
        "restore off withholds the durable seed from a connecting output"
    );
    // The saved window still came back.
    let restored = suspended_in_order(&mut f);
    assert_eq!(restored.len(), 1, "the saved window materialized");
    assert_eq!(restored[0].0.identity.app_id, "good");

    // The output connects at its default centered camera, not the saved one:
    // the real connect path seeds from `saved_camera_state`, which gates the
    // durable seed off.
    let seed = f.state().saved_camera_state();
    let (output, _global) =
        super::headless::add_output_with_saved(f.state(), 1, (1920, 1080), &seed);
    let (camera, zoom) = {
        let os = crate::state::output_state(&output);
        (os.camera, os.zoom)
    };
    assert_eq!(
        camera,
        Point::from((-960.0, -540.0)),
        "default centered camera"
    );
    assert_eq!(zoom, 1.0, "default zoom");

    for (s, _) in restored {
        f.state().dismiss_suspended(s.id);
    }
}

/// With `restore_camera` off, a durable camera for an output that is NOT
/// connected this session survives a steady-state rewrite — the write side
/// carries it forward, so flipping the flag on later still restores it (the
/// docs' "cameras are always saved regardless" promise).
#[test]
fn restore_camera_off_preserves_disconnected_output_camera() {
    let tmp = TempDir::new();
    let path = tmp.path().join("session.json");

    // A camera for an external monitor that won't be connected this boot.
    let mut outputs = BTreeMap::new();
    outputs.insert(
        "HEADLESS-2".to_string(),
        SessionOutput {
            camera: [-1234.0, -5678.0],
            zoom: 0.75,
        },
    );
    let envelope = SessionEnvelope {
        version: session::VERSION,
        bookmarks: BTreeMap::new(),
        saved_at: 0,
        entries: Vec::new(),
        outputs,
    };
    session::write(&path, &envelope, false).unwrap();

    // Default config: restore_camera off.
    let mut f = Fixture::with_config(Config::default());
    assert!(!f.state().config.session.restore_camera);
    // Only HEADLESS-1 connects; HEADLESS-2 stays absent this session.
    f.add_output(1, (1920, 1080));
    f.state().session_store.path = Some(path.clone());
    f.state().load_session();

    // A steady-state rewrite — what any suspend / dismiss / move triggers.
    f.state().session_store_write_now();

    let after = session::read(&path);
    let saved = after
        .outputs
        .get("HEADLESS-2")
        .expect("the disconnected output's camera survived the rewrite");
    assert_eq!(saved.camera, [-1234.0, -5678.0]);
    assert_eq!(saved.zoom, 0.75);
}

/// A create writes the durable file immediately; a dismiss rewrites it. Drives
/// the real conversion path, not the test-only insertion hook.
#[test]
fn create_and_dismiss_write_immediately() {
    let cache = TempDir::new();
    let tmp = TempDir::new();
    let path = tmp.path().join("session.json");

    let mut f = Fixture::with_config(config_restore(false));
    f.add_output(1, (1920, 1080));
    inject_cache(&mut f, &cache, &["myapp"]);
    f.state().session_store.path = Some(path.clone());

    let id = f.add_client();
    map_at(&mut f, id, "myapp", (400, 300), (300, 300));
    let window = window_by_app_id(&mut f, "myapp").unwrap();
    let serial = smithay::utils::SERIAL_COUNTER.next_serial();
    f.state().raise_and_focus(&window, serial);
    let surface = f.client(id).state.windows[0].surface.clone();

    // Suspend → convert → immediate write.
    f.state()
        .execute_action(&driftwm::config::Action::SuspendWindow);
    f.client(id).window(&surface).destroy();
    f.roundtrip(id);
    f.dispatch();

    let after_create = session::read(&path);
    assert_eq!(
        after_create.entries.len(),
        1,
        "create wrote through at once"
    );
    assert_eq!(after_create.entries[0].app_id, "myapp");
    assert_eq!(after_create.entries[0].origin, Origin::Explicit);

    let sid = after_create.entries[0].id;
    f.state().dismiss_suspended(crate::state::SuspendedId(sid));
    let after_dismiss = session::read(&path);
    assert!(
        after_dismiss.entries.is_empty(),
        "dismiss wrote through at once"
    );
}

/// The `csd` flag round-trips through session.json: a CSD window suspends to a
/// stand-in that records its client-decorated origin, the durable write keeps
/// it, and a fresh compositor materializes it as CSD-origin regardless of its
/// own decoration default.
#[test]
fn csd_flag_round_trips_through_session() {
    let cache = TempDir::new();
    let tmp = TempDir::new();
    let path = tmp.path().join("session.json");

    {
        let mut f = Fixture::with_config(
            Config::from_toml("[decorations]\ndefault_mode = \"client\"\n").unwrap(),
        );
        f.add_output(1, (1920, 1080));
        inject_cache(&mut f, &cache, &["myapp"]);
        f.state().session_store.path = Some(path.clone());
        let id = f.add_client();
        map_at(&mut f, id, "myapp", (400, 300), (300, 300));
        let window = window_by_app_id(&mut f, "myapp").unwrap();
        let serial = smithay::utils::SERIAL_COUNTER.next_serial();
        f.state().raise_and_focus(&window, serial);
        let surface = f.client(id).state.windows[0].surface.clone();
        f.state()
            .execute_action(&driftwm::config::Action::SuspendWindow);
        f.client(id).window(&surface).destroy();
        f.roundtrip(id);
        f.dispatch();

        // Tear the stand-in down cleanly for the fixture baseline, but keep the
        // durable file (clear the path so the dismiss doesn't rewrite it empty).
        let sid = suspended_in_order(&mut f)[0].0.id;
        f.state().session_store.path = None;
        f.state().dismiss_suspended(sid);
    }

    let saved = session::read(&path);
    assert_eq!(saved.entries.len(), 1);
    assert!(
        saved.entries[0].csd,
        "the file records the CSD-origin stand-in"
    );

    // A fresh compositor (whose own default is SSD) materializes it CSD-origin:
    // the flag rides on the entry, not the restoring config.
    let mut f = Fixture::with_config(config_restore(true));
    f.add_output(1, (1920, 1080));
    f.state().session_store.path = Some(path.clone());
    f.state().load_session();
    let restored = suspended_in_order(&mut f);
    assert_eq!(restored.len(), 1);
    assert!(restored[0].0.csd, "the restored stand-in stays CSD-origin");

    f.state().dismiss_suspended(restored[0].0.id);
}

/// A winit dev session skips persistence entirely unless overridden, and a
/// fixture without an injected path likewise never writes.
#[test]
fn no_path_disables_persistence() {
    let mut f = Fixture::with_config(config_restore(true));
    f.add_output(1, (1920, 1080));
    // No path injected: every write path is a no-op and touches no file.
    f.state().session_store.path = None;
    f.state().session_store_write_now();
    f.state().session_store_mark_dirty();
    f.state().serialize_session_on_shutdown();
    // Nothing to assert beyond "no panic, no file" — the fixture's teardown
    // baseline confirms no state leaked (e.g. a stray debounce timer).
    assert!(f.state().session_store.path.is_none());
}
