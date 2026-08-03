//! Durable session store + restore: the quit-serialize round-trip, origin
//! filtering with carry-forward when `restore_windows` is off, fresh-boot camera
//! seeding, and the immediate write on create/dismiss. The fixture drives the
//! same `serialize_session_on_shutdown` the main.rs choke point calls; the
//! post-`run()` wiring itself (Quit + signalfd both reaching it) is hardware
//! smoke, not covered here.

use std::collections::{BTreeMap, HashMap};
use std::rc::Rc;

use driftwm::config::Config;
use driftwm::desktop_entry::DesktopEntryCache;
use driftwm::session::{self, Origin, SessionEntry, SessionEnvelope, SessionOutput};
use smithay::utils::{Point, Rectangle, SERIAL_COUNTER, Size};

use crate::decorations::DecorationHit;
use crate::input::DecoTarget;
use crate::state::{CameraSeed, FocusTarget, StageWindow, SuspendedWindow};

use super::real::TempDir;
use super::{Fixture, map_window, server_surface, window_by_app_id};

/// SSD-on config with `[session].restore_windows` set as asked.
fn config_restore(on: bool) -> Config {
    Config::from_toml(&format!(
        "[session]\nrestore_windows = {on}\n[decorations]\ndefault_mode = \"server\"\n"
    ))
    .unwrap()
}

/// `config_restore`'s TOML plus an extra `[[window_rules]]` block appended
/// verbatim — as text, so a hot-reload can feed the compositor the same shape.
fn restore_toml(on: bool, rules_toml: &str) -> String {
    format!(
        "[session]\nrestore_windows = {on}\n[decorations]\ndefault_mode = \"server\"\n{rules_toml}"
    )
}

/// `config_restore` plus an extra `[[window_rules]]` block appended verbatim.
fn config_restore_with_rule(on: bool, rules_toml: &str) -> Config {
    Config::from_toml(&restore_toml(on, rules_toml)).unwrap()
}

/// Seat a desktop-entry cache resolving each `stem` to a launchable identity.
fn inject_cache(f: &mut Fixture, tmp: &TempDir, stems: &[&str]) {
    for stem in stems {
        let contents = format!("[Desktop Entry]\nType=Application\nName={stem}\nExec={stem}\n");
        std::fs::write(tmp.path().join(format!("{stem}.desktop")), contents).unwrap();
    }
    f.state().desktop_entry_cache = Some(DesktopEntryCache::new(vec![tmp.path().to_path_buf()]));
}

/// Map a client at `app_id`/`size` parked at a known canvas position. Returns
/// the client-side surface for later lookups.
fn map_at(
    f: &mut Fixture,
    id: super::client::ClientId,
    app_id: &str,
    size: (u16, u16),
    pos: (i32, i32),
) -> wayland_client::protocol::wl_surface::WlSurface {
    let surface = map_window(f, id, app_id, size);
    let window = window_by_app_id(f, app_id).unwrap();
    f.state()
        .map_window(StageWindow::Client(window), Point::from(pos), true);
    surface
}

/// Like `map_at`, but also sets a client-side title — for rules that match on
/// `title` as well as `app_id`. The title lands after the map, as a real
/// client's retitle does.
fn map_titled_at(
    f: &mut Fixture,
    id: super::client::ClientId,
    app_id: &str,
    title: &str,
    size: (u16, u16),
    pos: (i32, i32),
) {
    let surface = map_at(f, id, app_id, size, pos);
    let window = f.client(id).window(&surface);
    window.set_title(title);
    window.commit();
    f.roundtrip(id);
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
        position: [100, 200],
        size: [400, 300],
        origin,
        csd: false,
        focused: false,
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

/// The window focused at quit comes back as focus on its stand-in, so the first
/// window opened after a restart has an auto-placement anchor instead of landing
/// in the middle of the viewport.
#[test]
fn focus_round_trips_onto_the_restored_stand_in() {
    let tmp = TempDir::new();
    let path = tmp.path().join("session.json");

    {
        let cache = TempDir::new();
        let mut f = Fixture::with_config(config_restore(true));
        f.add_output(1, (1920, 1080));
        inject_cache(&mut f, &cache, &["alpha", "beta"]);
        f.state().session_store.path = Some(path.clone());

        let a = f.add_client();
        map_at(&mut f, a, "alpha", (400, 300), (-500, -200));
        let b = f.add_client();
        map_at(&mut f, b, "beta", (200, 200), (100, -200));

        // Focus alpha, the window *under* the last-mapped one, so the flag can't
        // be z-order in disguise.
        let alpha = window_by_app_id(&mut f, "alpha").unwrap();
        let serial = SERIAL_COUNTER.next_serial();
        f.state()
            .set_window_focus(Some(FocusTarget(server_surface(&alpha))), serial);

        f.state().serialize_session_on_shutdown();
    }

    let saved = session::read(&path);
    let flagged: Vec<&str> = saved
        .entries
        .iter()
        .filter(|e| e.focused)
        .map(|e| e.app_id.as_str())
        .collect();
    assert_eq!(flagged, vec!["alpha"], "only the focused window is flagged");

    let mut f = Fixture::with_config(config_restore(true));
    f.add_output(1, (1920, 1080));
    f.state().session_store.path = Some(path.clone());
    f.state().load_session();
    f.state().apply_restored_focus();

    let restored = suspended_in_order(&mut f);
    let alpha = restored
        .iter()
        .find(|(s, _)| s.identity.app_id == "alpha")
        .expect("alpha came back")
        .0
        .clone();
    assert_eq!(
        f.state().gated_suspended_focus(),
        Some(alpha.id),
        "the focused window's stand-in holds the focus"
    );
    assert!(
        matches!(
            f.state().focused_anchor_element(),
            Some(StageWindow::Suspended(s)) if s.id == alpha.id
        ),
        "the restored focus is the auto-placement anchor"
    );
    let order: Vec<&str> = restored
        .iter()
        .map(|(s, _)| s.identity.app_id.as_str())
        .collect();
    assert_eq!(
        order,
        vec!["alpha", "beta"],
        "granting the focus does not raise the stand-in — the saved z-order stands"
    );

    for (s, _) in restored {
        f.state().dismiss_suspended(s.id);
    }
}

/// A canvas left unfocused (the deliberate escape hatch for placing a window
/// wherever you like) comes back unfocused.
#[test]
fn an_unfocused_session_restores_unfocused() {
    let tmp = TempDir::new();
    let path = tmp.path().join("session.json");

    {
        let cache = TempDir::new();
        let mut f = Fixture::with_config(config_restore(true));
        f.add_output(1, (1920, 1080));
        inject_cache(&mut f, &cache, &["alpha"]);
        f.state().session_store.path = Some(path.clone());

        let a = f.add_client();
        map_at(&mut f, a, "alpha", (400, 300), (-500, -200));
        let serial = SERIAL_COUNTER.next_serial();
        f.state().set_window_focus(None, serial);

        f.state().serialize_session_on_shutdown();
    }

    let saved = session::read(&path);
    assert!(
        saved.entries.iter().all(|e| !e.focused),
        "nothing is flagged when nothing was focused"
    );

    let mut f = Fixture::with_config(config_restore(true));
    f.add_output(1, (1920, 1080));
    f.state().session_store.path = Some(path.clone());
    f.state().load_session();
    f.state().apply_restored_focus();

    assert_eq!(f.state().gated_suspended_focus(), None);
    for (s, _) in suspended_in_order(&mut f) {
        f.state().dismiss_suspended(s.id);
    }
}

/// A restored focus lands only on a stand-in you can actually see. Off-screen —
/// the camera didn't come back with it, or you quit panned away — the canvas
/// starts unfocused rather than pointing relaunch and dismiss at a window
/// nothing on screen shows.
#[test]
fn an_off_screen_restored_focus_is_withheld() {
    let tmp = TempDir::new();
    let path = tmp.path().join("session.json");

    let mut far = entry(1, "faraway", Origin::Explicit);
    far.position = [40_000, 40_000];
    far.focused = true;
    let envelope = SessionEnvelope {
        version: session::VERSION,
        bookmarks: BTreeMap::new(),
        saved_at: 0,
        entries: vec![far],
        outputs: BTreeMap::new(),
    };
    session::write(&path, &envelope, false).unwrap();

    let mut f = Fixture::with_config(config_restore(true));
    f.add_output(1, (1920, 1080));
    f.state().session_store.path = Some(path.clone());
    f.state().load_session();
    f.state().apply_restored_focus();

    let restored = suspended_in_order(&mut f);
    assert_eq!(restored.len(), 1, "the stand-in itself still comes back");
    assert_eq!(
        f.state().gated_suspended_focus(),
        None,
        "focus is not handed to a stand-in outside the viewport"
    );

    for (s, _) in restored {
        f.state().dismiss_suspended(s.id);
    }
}

/// A withheld hand-over must not destroy the record. Every write re-emits the
/// flag while nothing else has taken focus, so the boot after — one whose camera
/// actually frames the stand-in — still finds it. Without this, the ordinary
/// `restore_camera = false` boot erases the flag on its first write and the
/// feature silently self-destructs.
#[test]
fn a_withheld_restored_focus_survives_to_the_next_boot() {
    let tmp = TempDir::new();
    let path = tmp.path().join("session.json");

    let mut far = entry(1, "faraway", Origin::Explicit);
    far.position = [40_000, 40_000];
    far.focused = true;
    let envelope = SessionEnvelope {
        version: session::VERSION,
        bookmarks: BTreeMap::new(),
        saved_at: 0,
        entries: vec![far],
        outputs: BTreeMap::new(),
    };
    session::write(&path, &envelope, false).unwrap();

    // Boot one: the default camera leaves the stand-in off screen, so the focus
    // is withheld — and then the session is written straight back out.
    {
        let mut f = Fixture::with_config(config_restore(true));
        // This boot quits with its stand-in still on the canvas, as a real one
        // does — dismissing it to reach the baseline would rewrite the file the
        // second boot reads.
        f.skip_baseline_check();
        f.add_output(1, (1920, 1080));
        f.state().session_store.path = Some(path.clone());
        f.state().load_session();
        f.state().apply_restored_focus();
        assert_eq!(
            f.state().gated_suspended_focus(),
            None,
            "the off-screen stand-in is not focused"
        );
        f.state().serialize_session_on_shutdown();
    }

    let rewritten = session::read(&path);
    let flagged: Vec<&str> = rewritten
        .entries
        .iter()
        .filter(|e| e.focused)
        .map(|e| e.app_id.as_str())
        .collect();
    assert_eq!(
        flagged,
        vec!["faraway"],
        "the rewrite keeps the record a withheld hand-over left pending"
    );

    // Boot two, camera parked on the stand-in: the surviving flag is what the
    // focus is granted from.
    let mut f = Fixture::with_config(config_restore(true));
    // Canvas coords are center-based and y-up, so the entry's [40_000, 40_000]
    // sits at internal (39_800, -40_150): frame it from a little up-left of that.
    let saved = HashMap::from([(
        "HEADLESS-1".to_string(),
        (CameraSeed::Camera(Point::from((39_600.0, -40_350.0))), 1.0),
    )]);
    super::headless::add_output_with_saved(f.state(), 1, (1920, 1080), &saved);
    f.state().session_store.path = Some(path.clone());
    f.state().load_session();
    f.state().apply_restored_focus();

    let restored = suspended_in_order(&mut f);
    assert_eq!(restored.len(), 1);
    assert_eq!(
        f.state().gated_suspended_focus(),
        Some(restored[0].0.id),
        "the next boot that can see the stand-in grants the focus"
    );

    for (s, _) in restored {
        f.state().dismiss_suspended(s.id);
    }
}

/// A carried-forward entry's focus flag belongs to a boot that's over: it's
/// cleared on the rewrite, so flipping `restore_windows` on later can't restore
/// focus onto a window from two sessions ago.
#[test]
fn a_carried_entry_loses_its_stale_focus_flag() {
    let tmp = TempDir::new();
    let path = tmp.path().join("session.json");

    let mut stale = entry(2, "onlyquit", Origin::Quit);
    stale.focused = true;
    let envelope = SessionEnvelope {
        version: session::VERSION,
        bookmarks: BTreeMap::new(),
        saved_at: 0,
        entries: vec![entry(1, "keepme", Origin::Explicit), stale],
        outputs: BTreeMap::new(),
    };
    session::write(&path, &envelope, false).unwrap();

    // Restore off: the quit entry is carried, not materialized.
    let mut f = Fixture::with_config(config_restore(false));
    f.add_output(1, (1920, 1080));
    f.state().session_store.path = Some(path.clone());
    f.state().load_session();
    f.state().apply_restored_focus();
    assert_eq!(
        f.state().gated_suspended_focus(),
        None,
        "a carried entry's flag restores no focus — it never materialized"
    );

    // Dismissing the explicit stand-in rewrites the file.
    let restored = suspended_in_order(&mut f);
    assert_eq!(restored.len(), 1);
    assert_eq!(restored[0].0.identity.app_id, "keepme");
    f.state().dismiss_suspended(restored[0].0.id);

    let after = session::read(&path);
    assert_eq!(after.entries.len(), 1);
    assert_eq!(after.entries[0].app_id, "onlyquit");
    assert!(
        !after.entries[0].focused,
        "the carried entry is re-emitted unflagged"
    );
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
    let seed = f.state().saved_camera_state();
    let (output, _global) =
        super::headless::add_output_with_saved(f.state(), 1, (1920, 1080), &seed);
    let (camera, zoom) = {
        let os = crate::state::output_state(&output);
        (os.camera, os.zoom)
    };
    assert_eq!(camera, Point::from((-1234.0, -5678.0)));
    assert_eq!(zoom, 0.75);
}

/// The runtime state file publishes each output's viewport centre, so a seed
/// from it restores the viewport that centre framed — at a zoom where centre
/// and internal camera are far apart.
#[test]
fn center_seed_restores_the_framed_viewport() {
    let mut f = Fixture::with_config(Config::default());
    let logical = Size::from((1920, 1080));
    let camera = Point::from((-1234.0, -5678.0));
    let zoom = 0.75;
    let (x, y) = driftwm::canvas::viewport_center(camera, zoom, logical);

    let saved = HashMap::from([(
        "HEADLESS-1".to_string(),
        (CameraSeed::Center { x, y }, zoom),
    )]);
    let (output, _global) =
        super::headless::add_output_with_saved(f.state(), 1, (1920, 1080), &saved);

    let (restored, restored_zoom) = {
        let os = crate::state::output_state(&output);
        (os.camera, os.zoom)
    };
    assert!(
        (restored.x - camera.x).abs() < 1e-6 && (restored.y - camera.y).abs() < 1e-6,
        "restored camera {restored:?} does not frame the published centre"
    );
    assert_eq!(restored_zoom, zoom);
}

/// The seed is checked after it resolves, so the bounds guard the camera the
/// output actually takes. A centre that is itself inside the canvas limit but
/// lands outside it once the half-viewport is subtracted is refused, and the
/// output keeps its default viewport.
#[test]
fn a_center_seed_resolving_out_of_range_is_refused() {
    let mut f = Fixture::with_config(Config::default());
    let saved = HashMap::from([(
        "HEADLESS-1".to_string(),
        (CameraSeed::Center { x: -1e9, y: 0.0 }, 0.5),
    )]);
    let (output, _global) =
        super::headless::add_output_with_saved(f.state(), 1, (1920, 1080), &saved);

    let (camera, zoom) = {
        let os = crate::state::output_state(&output);
        (os.camera, os.zoom)
    };
    assert_eq!(camera, Point::from((-960.0, -540.0)), "default camera");
    assert_eq!(zoom, 1.0, "default zoom");
}

/// A `zoom: 0.0` centre seed (hand-edit / corruption in the runtime state file,
/// which validates nothing itself) divides the conversion to infinity. The
/// output falls back to its default viewport instead of taking an inf camera.
#[test]
fn a_corrupt_zoom_center_seed_is_refused() {
    let mut f = Fixture::with_config(Config::default());
    let saved = HashMap::from([(
        "HEADLESS-1".to_string(),
        (CameraSeed::Center { x: 0.0, y: 0.0 }, 0.0),
    )]);
    let (output, _global) =
        super::headless::add_output_with_saved(f.state(), 1, (1920, 1080), &saved);

    let (camera, zoom) = {
        let os = crate::state::output_state(&output);
        (os.camera, os.zoom)
    };
    assert_eq!(camera, Point::from((-960.0, -540.0)), "default camera");
    assert_eq!(zoom, 1.0, "default zoom");
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

/// A schema-v1 file's `position`/`size` describe the stand-in's bare body (no
/// chrome). Loading it converts those numbers to the v2 frame convention and
/// deflates them back to the same body, so a v1 record materializes at exactly
/// the rect it always described, wearing its chrome around it.
#[test]
fn v1_session_entry_converts_body_to_frame_on_load() {
    let tmp = TempDir::new();
    let path = tmp.path().join("session.json");
    std::fs::write(
        &path,
        r#"{"version":1,"saved_at":0,"outputs":{},"entries":[
            {"id":1,"app_id":"legacy","desktop_id":"legacy.desktop","display_name":"Legacy",
             "position":[100,200],"size":[400,300],"origin":"explicit"}]}"#,
    )
    .unwrap();

    let mut f =
        Fixture::with_config(Config::from_toml("[decorations]\nborder_width = 4\n").unwrap());
    f.add_output(1, (1920, 1080));
    f.state().session_store.path = Some(path.clone());
    f.state().load_session();

    let restored = suspended_in_order(&mut f);
    assert_eq!(restored.len(), 1);
    let (s, pos) = (restored[0].0.clone(), restored[0].1);

    // The v1 numbers describe the body directly: content top-left =
    // rule_to_internal(100, 200, (400, 300)) = (100 - 200, -200 - 150) =
    // (-100, -350). The conversion inflates that by the 25px bar and 4px border
    // into a frame and `materialize_entry` deflates it straight back. Reading
    // the v1 numbers as a frame instead would shrink the body to 392×267 at
    // (-96, -321).
    assert_eq!(
        s.size.get(),
        Size::from((400, 300)),
        "the stand-in's body is the v1 size verbatim, not shrunk by the chrome"
    );
    assert_eq!(pos, Point::from((-100, -350)));

    f.state().dismiss_suspended(s.id);
}

/// The same visible frame as the v1 file above, but already in the v2
/// convention: no migration runs (`envelope.version == session::VERSION`), so
/// landing on the identical body and position proves those really are the
/// current on-disk numbers, not an artifact of the v1 conversion.
#[test]
fn v2_session_entry_round_trips_unchanged() {
    let tmp = TempDir::new();
    let path = tmp.path().join("session.json");
    let envelope = SessionEnvelope {
        version: session::VERSION,
        bookmarks: BTreeMap::new(),
        saved_at: 0,
        entries: vec![SessionEntry {
            id: 1,
            app_id: "current".to_string(),
            desktop_id: "current.desktop".to_string(),
            display_name: "Current".to_string(),
            position: [100, 213],
            size: [408, 333],
            origin: Origin::Explicit,
            csd: false,
            focused: false,
        }],
        outputs: BTreeMap::new(),
    };
    session::write(&path, &envelope, false).unwrap();

    let mut f =
        Fixture::with_config(Config::from_toml("[decorations]\nborder_width = 4\n").unwrap());
    f.add_output(1, (1920, 1080));
    f.state().session_store.path = Some(path.clone());
    f.state().load_session();

    let restored = suspended_in_order(&mut f);
    assert_eq!(restored.len(), 1);
    let (s, pos) = (restored[0].0.clone(), restored[0].1);
    assert_eq!(s.size.get(), Size::from((400, 300)));
    assert_eq!(pos, Point::from((-100, -350)));

    f.state().dismiss_suspended(s.id);
}

/// A version this build doesn't recognize (too new, or garbled) is
/// quarantined at the `DriftWm::load_session` boundary, not just at the
/// lower-level `session::read` the lib-crate unit tests already cover:
/// nothing materializes, and the bad file is renamed aside rather than
/// silently misread.
#[test]
fn unknown_session_version_is_quarantined_on_load() {
    let tmp = TempDir::new();
    let path = tmp.path().join("session.json");
    std::fs::write(
        &path,
        r#"{"version":999,"saved_at":0,"outputs":{},"entries":[]}"#,
    )
    .unwrap();

    let mut f = Fixture::with_config(config_restore(true));
    f.add_output(1, (1920, 1080));
    f.state().session_store.path = Some(path.clone());
    f.state().load_session();

    assert_eq!(suspended_in_order(&mut f).len(), 0);
    assert!(
        !path.exists(),
        "a future-version file is quarantined, not silently misread"
    );
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
    let seed = f.state().saved_camera_state();
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
    assert!(
        after_create.entries[0].focused,
        "the stand-in inherited the closed window's focus, and the write kept it"
    );

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

/// A `restore_windows = false` rule keeps its app's live window out of the
/// shutdown save even with the global flag on, while an unruled app's live
/// window still saves — proving the exclusion is the rule, not a missing
/// desktop entry or some other blanket ineligibility.
#[test]
fn restore_windows_false_rule_excludes_matching_app_from_shutdown_save() {
    let cache = TempDir::new();
    let tmp = TempDir::new();
    let path = tmp.path().join("session.json");

    let config = config_restore_with_rule(
        true,
        "[[window_rules]]\napp_id = \"excluded\"\nrestore_windows = false\n",
    );
    let mut f = Fixture::with_config(config);
    f.add_output(1, (1920, 1080));
    inject_cache(&mut f, &cache, &["excluded", "included"]);
    f.state().session_store.path = Some(path.clone());

    let a = f.add_client();
    map_at(&mut f, a, "excluded", (400, 300), (300, 300));
    let b = f.add_client();
    map_at(&mut f, b, "included", (400, 300), (800, 300));

    f.state().serialize_session_on_shutdown();

    let saved = session::read(&path);
    assert!(
        saved.entries.iter().all(|e| e.app_id != "excluded"),
        "the ruled-out app's live window is not saved despite the global flag being on"
    );
    assert!(
        saved.entries.iter().any(|e| e.app_id == "included"),
        "the unruled app's live window still saves"
    );
}

/// A `restore_windows = false` rule keeps a pre-existing `Quit` record from
/// materializing at load, but the record is carried forward inert (re-emitted,
/// not destroyed) since a carried entry is never itself materialized. The two
/// non-matching apps flanking it in the file still materialize, in order.
#[test]
fn restore_windows_false_rule_quit_record_carries_forward_inert() {
    let tmp = TempDir::new();
    let path = tmp.path().join("session.json");

    let envelope = SessionEnvelope {
        version: session::VERSION,
        bookmarks: BTreeMap::new(),
        saved_at: 0,
        entries: vec![
            entry(1, "alpha", Origin::Quit),
            entry(2, "excluded", Origin::Quit),
            entry(3, "beta", Origin::Quit),
        ],
        outputs: BTreeMap::new(),
    };
    session::write(&path, &envelope, false).unwrap();

    let config = config_restore_with_rule(
        true,
        "[[window_rules]]\napp_id = \"excluded\"\nrestore_windows = false\n",
    );
    let mut f = Fixture::with_config(config);
    f.add_output(1, (1920, 1080));
    f.state().session_store.path = Some(path.clone());
    f.state().load_session();

    let restored = suspended_in_order(&mut f);
    assert_eq!(
        restored.len(),
        2,
        "the ruled-out quit record never materializes"
    );
    assert_eq!(
        restored
            .iter()
            .map(|(s, _)| s.identity.app_id.clone())
            .collect::<Vec<_>>(),
        vec!["alpha", "beta"],
        "the surviving records keep their original relative order"
    );

    f.state().session_store_write_now();
    let after = session::read(&path);
    assert!(
        after.entries.iter().any(|e| e.app_id == "excluded"),
        "the ruled-out quit record is carried forward, not destroyed"
    );

    for (s, _) in restored {
        f.state().dismiss_suspended(s.id);
    }
}

/// Dropping the rule that excluded a carried `Quit` record lets it materialize
/// again on the next load: nothing about the earlier exclusion destroyed the
/// record, it just sat inert in the file.
#[test]
fn restore_windows_false_rule_removed_rematerializes_carried_quit() {
    let tmp = TempDir::new();
    let path = tmp.path().join("session.json");

    let envelope = SessionEnvelope {
        version: session::VERSION,
        bookmarks: BTreeMap::new(),
        saved_at: 0,
        entries: vec![entry(1, "excluded", Origin::Quit)],
        outputs: BTreeMap::new(),
    };
    session::write(&path, &envelope, false).unwrap();

    // Boot 1: the rule excludes it — carried forward inert, rewritten as-is.
    {
        let config = config_restore_with_rule(
            true,
            "[[window_rules]]\napp_id = \"excluded\"\nrestore_windows = false\n",
        );
        let mut f = Fixture::with_config(config);
        f.add_output(1, (1920, 1080));
        f.state().session_store.path = Some(path.clone());
        f.state().load_session();
        assert_eq!(
            suspended_in_order(&mut f).len(),
            0,
            "excluded while the rule is present"
        );
        f.state().session_store_write_now();
    }

    // Boot 2: the rule is gone — the very same record, untouched in the file,
    // comes back.
    let mut f = Fixture::with_config(config_restore(true));
    f.add_output(1, (1920, 1080));
    f.state().session_store.path = Some(path.clone());
    f.state().load_session();

    let restored = suspended_in_order(&mut f);
    assert_eq!(
        restored.len(),
        1,
        "dropping the rule restores the previously-excluded record"
    );
    assert_eq!(restored[0].0.identity.app_id, "excluded");

    f.state().dismiss_suspended(restored[0].0.id);
}

/// A rule keyed on both `app_id` and `title` is read off `app_id` alone at
/// load, since a saved record carries no title: the record sits inert instead of
/// materializing into a stand-in that would save itself again every cycle,
/// coming back forever against the rule. The title criterion still narrows the
/// save, where the live title is known.
#[test]
fn restore_windows_false_rule_with_title_excludes_records_by_app_id() {
    let cache = TempDir::new();
    let tmp = TempDir::new();
    let path = tmp.path().join("session.json");

    let envelope = SessionEnvelope {
        version: session::VERSION,
        bookmarks: BTreeMap::new(),
        saved_at: 0,
        entries: vec![entry(1, "excluded", Origin::Quit)],
        outputs: BTreeMap::new(),
    };
    session::write(&path, &envelope, false).unwrap();

    let config = config_restore_with_rule(
        true,
        "[[window_rules]]\napp_id = \"excluded\"\ntitle = \"Some Window\"\nrestore_windows = false\n",
    );
    let mut f = Fixture::with_config(config);
    f.add_output(1, (1920, 1080));
    inject_cache(&mut f, &cache, &["excluded"]);
    f.state().session_store.path = Some(path.clone());
    f.state().load_session();

    assert_eq!(
        suspended_in_order(&mut f).len(),
        0,
        "a record the rule can't tell apart by title is excluded on its app_id"
    );

    // The live window's real title is known at save time, so the same rule keeps
    // it out of the shutdown save — and the record is carried forward once, not
    // re-saved as a stand-in of its own.
    let id = f.add_client();
    map_titled_at(
        &mut f,
        id,
        "excluded",
        "Some Window",
        (400, 300),
        (300, 300),
    );

    f.state().serialize_session_on_shutdown();
    let after = session::read(&path);
    assert_eq!(
        after
            .entries
            .iter()
            .filter(|e| e.app_id == "excluded")
            .count(),
        1,
        "the live window stays out of the save, leaving just the carried record"
    );
    assert_eq!(
        after.entries[0].position,
        [100, 200],
        "that one record is the untouched carry, not a fresh save of the live window"
    );
}

/// A rule matching on `title` alone can't be keyed to a saved record — nothing
/// in the file carries a title — so it governs the save only and leaves what
/// comes back to the section key. Consulting it with the title unknown would
/// make it answer for every app instead.
#[test]
fn a_title_only_restore_windows_rule_does_not_govern_what_comes_back() {
    let tmp = TempDir::new();
    let path = tmp.path().join("session.json");

    let envelope = SessionEnvelope {
        version: session::VERSION,
        bookmarks: BTreeMap::new(),
        saved_at: 0,
        entries: vec![entry(1, "someapp", Origin::Quit)],
        outputs: BTreeMap::new(),
    };
    session::write(&path, &envelope, false).unwrap();

    let config = config_restore_with_rule(
        true,
        "[[window_rules]]\ntitle = \"Some Window\"\nrestore_windows = false\n",
    );
    let mut f = Fixture::with_config(config);
    f.add_output(1, (1920, 1080));
    f.state().session_store.path = Some(path.clone());
    f.state().load_session();

    let restored = suspended_in_order(&mut f);
    assert_eq!(
        restored.len(),
        1,
        "an unkeyable rule leaves the record to the section key"
    );

    f.state().dismiss_suspended(restored[0].0.id);
}

/// An explicitly suspended stand-in is saved at shutdown even for an app a
/// `restore_windows = false` rule keeps out of the save: the rule governs the
/// automatic save of still-open windows, not an artifact the user deliberately
/// left on the canvas — which is what the load side's `Explicit` bypass expects
/// to find in the file.
#[test]
fn restore_windows_false_rule_still_saves_an_explicit_stand_in() {
    let cache = TempDir::new();
    let tmp = TempDir::new();
    let path = tmp.path().join("session.json");

    let config = config_restore_with_rule(
        true,
        "[[window_rules]]\napp_id = \"excluded\"\nrestore_windows = false\n",
    );
    let mut f = Fixture::with_config(config);
    f.add_output(1, (1920, 1080));
    inject_cache(&mut f, &cache, &["excluded"]);
    f.state().session_store.path = Some(path.clone());

    let id = f.add_client();
    let surface = map_at(&mut f, id, "excluded", (400, 300), (300, 300));
    let window = window_by_app_id(&mut f, "excluded").unwrap();
    let serial = SERIAL_COUNTER.next_serial();
    f.state().raise_and_focus(&window, serial);
    f.state()
        .execute_action(&driftwm::config::Action::SuspendWindow);
    f.client(id).window(&surface).destroy();
    f.roundtrip(id);
    f.dispatch();

    f.state().serialize_session_on_shutdown();

    let saved = session::read(&path);
    assert_eq!(saved.entries.len(), 1);
    assert_eq!(saved.entries[0].app_id, "excluded");
    assert_eq!(
        saved.entries[0].origin,
        Origin::Explicit,
        "the deliberate stand-in is saved despite the rule"
    );

    let sid = suspended_in_order(&mut f)[0].0.id;
    f.state().dismiss_suspended(sid);
}

/// `restore_windows` is resolved against the live config, not the rule stamped
/// when a window mapped, so a rule added or dropped by a hot-reload decides the
/// next shutdown save without either window remapping.
#[test]
fn a_hot_reloaded_restore_windows_rule_decides_the_next_save() {
    let cache = TempDir::new();
    let tmp = TempDir::new();
    let path = tmp.path().join("session.json");

    let rule_for =
        |app: &str| format!("[[window_rules]]\napp_id = \"{app}\"\nrestore_windows = false\n");

    let mut f = Fixture::with_config(config_restore_with_rule(true, &rule_for("alpha")));
    f.add_output(1, (1920, 1080));
    inject_cache(&mut f, &cache, &["alpha", "beta"]);
    f.state().session_store.path = Some(path.clone());

    let a = f.add_client();
    map_at(&mut f, a, "alpha", (400, 300), (300, 300));
    let b = f.add_client();
    map_at(&mut f, b, "beta", (400, 300), (800, 300));

    // Swap which app the rule excludes while both windows stay mapped.
    f.state()
        .reload_config_from_contents(&restore_toml(true, &rule_for("beta")));

    f.state().serialize_session_on_shutdown();

    let saved = session::read(&path);
    assert!(
        saved.entries.iter().any(|e| e.app_id == "alpha"),
        "the app the reload stopped excluding is saved"
    );
    assert!(
        saved.entries.iter().all(|e| e.app_id != "beta"),
        "the app the reload started excluding is not"
    );

    // The headless fixture has no backend to drain a queued mode intent.
    f.state().pending_mode_changes.clear();
}

/// A `restore_windows = false` rule does not touch `Explicit`-origin records:
/// a deliberately suspended stand-in for that same app still materializes.
#[test]
fn restore_windows_false_rule_still_materializes_explicit_entry() {
    let tmp = TempDir::new();
    let path = tmp.path().join("session.json");

    let envelope = SessionEnvelope {
        version: session::VERSION,
        bookmarks: BTreeMap::new(),
        saved_at: 0,
        entries: vec![entry(1, "excluded", Origin::Explicit)],
        outputs: BTreeMap::new(),
    };
    session::write(&path, &envelope, false).unwrap();

    let config = config_restore_with_rule(
        true,
        "[[window_rules]]\napp_id = \"excluded\"\nrestore_windows = false\n",
    );
    let mut f = Fixture::with_config(config);
    f.add_output(1, (1920, 1080));
    f.state().session_store.path = Some(path.clone());
    f.state().load_session();

    let restored = suspended_in_order(&mut f);
    assert_eq!(
        restored.len(),
        1,
        "an explicit entry materializes regardless of the restore_windows rule"
    );
    assert_eq!(restored[0].0.identity.app_id, "excluded");

    f.state().dismiss_suspended(restored[0].0.id);
}

/// A `restore_windows = true` rule saves and materializes its app even with
/// the global flag off, while an unruled app's live window still doesn't
/// save, and an unrelated pre-existing carried `Quit` record is untouched.
#[test]
fn restore_windows_true_rule_saves_and_materializes_with_global_off() {
    let cache = TempDir::new();
    let tmp = TempDir::new();
    let path = tmp.path().join("session.json");

    // A prior session left a quit entry for an app this test never touches.
    let envelope = SessionEnvelope {
        version: session::VERSION,
        bookmarks: BTreeMap::new(),
        saved_at: 0,
        entries: vec![entry(1, "untouched", Origin::Quit)],
        outputs: BTreeMap::new(),
    };
    session::write(&path, &envelope, false).unwrap();

    let rules_toml = "[[window_rules]]\napp_id = \"included\"\nrestore_windows = true\n";
    let mut f = Fixture::with_config(config_restore_with_rule(false, rules_toml));
    f.add_output(1, (1920, 1080));
    inject_cache(&mut f, &cache, &["included", "excluded", "untouched"]);
    f.state().session_store.path = Some(path.clone());
    f.state().load_session();

    // The global flag is off and "untouched" has no rule, so its quit entry
    // carries forward unmaterialized.
    assert_eq!(
        suspended_in_order(&mut f).len(),
        0,
        "nothing materializes at load"
    );

    let a = f.add_client();
    map_at(&mut f, a, "included", (400, 300), (300, 300));
    let b = f.add_client();
    map_at(&mut f, b, "excluded", (400, 300), (800, 300));

    f.state().serialize_session_on_shutdown();

    let after = session::read(&path);
    assert!(
        after
            .entries
            .iter()
            .any(|e| e.app_id == "included" && e.origin == Origin::Quit),
        "the ruled-in app's live window is saved despite the global flag being off"
    );
    assert!(
        after.entries.iter().all(|e| e.app_id != "excluded"),
        "the unruled app's live window is not saved while the global flag is off"
    );
    assert!(
        after
            .entries
            .iter()
            .any(|e| e.app_id == "untouched" && e.origin == Origin::Quit),
        "the unrelated carried quit record is preserved"
    );

    // A fresh load materializes the rule-included app from the same file.
    let mut f2 = Fixture::with_config(config_restore_with_rule(false, rules_toml));
    f2.add_output(1, (1920, 1080));
    f2.state().session_store.path = Some(path.clone());
    f2.state().load_session();
    let restored = suspended_in_order(&mut f2);
    assert!(
        restored
            .iter()
            .any(|(s, _)| s.identity.app_id == "included"),
        "the ruled-in app materializes on the next load despite the global flag being off"
    );

    for (s, _) in restored {
        f2.state().dismiss_suspended(s.id);
    }
}

/// A `restore_windows = false` rule's carried record doesn't accumulate across
/// repeated login/logout cycles: the file always shows exactly the one record
/// for the ruled app, never growing by one per logout, while an unruled app's
/// live window keeps saving fresh every cycle.
#[test]
fn restore_windows_false_rule_carried_record_does_not_grow_across_cycles() {
    let cache = TempDir::new();
    let tmp = TempDir::new();
    let path = tmp.path().join("session.json");

    let envelope = SessionEnvelope {
        version: session::VERSION,
        bookmarks: BTreeMap::new(),
        saved_at: 0,
        entries: vec![entry(1, "excluded", Origin::Quit)],
        outputs: BTreeMap::new(),
    };
    session::write(&path, &envelope, false).unwrap();

    let rules_toml = "[[window_rules]]\napp_id = \"excluded\"\nrestore_windows = false\n";

    for cycle in 0..3 {
        let included_app = format!("included-{cycle}");
        let mut f = Fixture::with_config(config_restore_with_rule(true, rules_toml));
        f.add_output(1, (1920, 1080));
        inject_cache(&mut f, &cache, &["excluded", included_app.as_str()]);
        f.state().session_store.path = Some(path.clone());
        f.state().load_session();
        // A previous cycle's unruled entry materializes as a dormant stand-in
        // now that it's a Quit record with the global flag on; dismiss it at
        // the end of this cycle so the fixture's leak check stays clean.
        let leftover = suspended_in_order(&mut f);

        let a = f.add_client();
        map_at(&mut f, a, "excluded", (400, 300), (300, 300));
        let b = f.add_client();
        map_at(&mut f, b, &included_app, (400, 300), (800, 300));

        f.state().serialize_session_on_shutdown();

        let after = session::read(&path);
        let excluded_count = after
            .entries
            .iter()
            .filter(|e| e.app_id == "excluded")
            .count();
        assert_eq!(
            excluded_count, 1,
            "cycle {cycle}: the ruled-out app's carried record count stays fixed, not growing"
        );
        assert!(
            after
                .entries
                .iter()
                .any(|e| e.app_id == included_app && e.origin == Origin::Quit),
            "cycle {cycle}: the unruled app's live window is saved"
        );

        for (s, _) in leftover {
            f.state().dismiss_suspended(s.id);
        }
    }
}
