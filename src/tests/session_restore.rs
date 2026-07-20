//! Durable session store + restore: the quit-serialize round-trip, origin
//! filtering with carry-forward when `restore_session` is off, fresh-boot camera
//! seeding, and the immediate write on create/dismiss. The fixture drives the
//! same `serialize_session_on_shutdown` the main.rs choke point calls; the
//! post-`run()` wiring itself (Quit + signalfd both reaching it) is hardware
//! smoke, not covered here.

use std::collections::BTreeMap;
use std::rc::Rc;

use driftwm::config::Config;
use driftwm::desktop_entry::DesktopEntryCache;
use driftwm::session::{self, Origin, SessionEntry, SessionEnvelope, SessionOutput};
use smithay::utils::{Point, Size};

use crate::state::{StageWindow, SuspendedWindow};

use super::real::TempDir;
use super::{Fixture, map_window, window_by_app_id};

/// SSD-on config with `restore_session` set as asked.
fn config_restore(on: bool) -> Config {
    Config::from_toml(&format!(
        "restore_session = {on}\n[decorations]\ndefault_mode = \"server\"\n"
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
    }
}

/// Serialize live windows on quit (`restore_session = true`), then a fresh
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

    // A fresh compositor materializes them in order at the same rects.
    let mut f = Fixture::with_config(config_restore(true));
    f.add_output(1, (1920, 1080));
    f.state().session_store.path = Some(path.clone());
    f.state().load_session();

    let restored = suspended_in_order(&mut f);
    assert_eq!(restored.len(), 2);
    assert_eq!(restored[0].0.identity.app_id, "alpha");
    assert_eq!(restored[0].1, Point::from((500, 500)));
    assert_eq!(restored[0].0.size.get(), Size::from((400, 300)));
    assert_eq!(restored[0].0.origin, Origin::Quit);
    assert_eq!(restored[1].0.identity.app_id, "beta");
    assert_eq!(restored[1].1, Point::from((-300, 100)));
    assert_eq!(restored[1].0.size.get(), Size::from((200, 200)));

    for (s, _) in restored {
        f.state().dismiss_suspended(s.id);
    }
}

/// With `restore_session` off, an explicit entry materializes but a quit entry
/// does not — and the quit entry is carried forward on the next rewrite, so a
/// flag-off session never destroys the saved session.
#[test]
fn flag_off_materializes_explicit_and_carries_quit() {
    let tmp = TempDir::new();
    let path = tmp.path().join("session.json");

    // A prior session saved one explicit + one quit entry.
    let envelope = SessionEnvelope {
        version: session::VERSION,
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

/// A durable per-output camera seeds a freshly connected output on fresh boot
/// (no runtime entry). Runtime-wins is exercised by the `merge_saved_cameras`
/// unit test.
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
            // and out-of-bounds seeds are now rejected on load.
            zoom: 0.75,
        },
    );
    let envelope = SessionEnvelope {
        version: session::VERSION,
        saved_at: 0,
        entries: Vec::new(),
        outputs,
    };
    session::write(&path, &envelope, false).unwrap();

    let mut f = Fixture::with_config(config_restore(false));
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
        saved_at: 0,
        entries: Vec::new(),
        outputs,
    };
    session::write(&path, &envelope, false).unwrap();

    let mut f = Fixture::with_config(config_restore(false));
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
