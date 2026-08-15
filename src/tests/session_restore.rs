//! Durable session store + restore: the save/restore round-trip, origin
//! filtering with carry-forward when `restore_windows` is off, fresh-boot camera
//! seeding, and what arms the debounce. The debounced flush is the only writer
//! — nothing writes at shutdown — and it records live windows as well as
//! stand-ins, so a crash or a logout that kills the clients leaves the canvas
//! as of the last flush in the file.
//!
//! That flush is a real calloop timer with no injectable clock, so scenarios
//! drive `session_store_write_now` where production would wait the debounce
//! out. An "assert dirty" therefore has to flush first: mapping a toplevel and
//! plenty of other paths set the flag, and only a flush clears it. The timer
//! runs on either of two intervals — the window one, or the longer camera one —
//! and the scenarios that care assert which, never a wall-clock value.

use std::collections::{BTreeMap, HashMap};
use std::rc::Rc;

use driftwm::config::Config;
use driftwm::desktop_entry::DesktopEntryCache;
use driftwm::session::{self, Origin, SessionEntry, SessionEnvelope, SessionOutput};
use smithay::utils::{Point, Rectangle, SERIAL_COUNTER, Size};
use wayland_protocols_wlr::layer_shell::v1::client::{zwlr_layer_shell_v1, zwlr_layer_surface_v1};

use crate::decorations::DecorationHit;
use crate::input::DecoTarget;
use crate::state::{CameraSeed, FocusTarget, StageWindow, SuspendedWindow, WRITE_DEBOUNCE};

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

/// A flush records live windows (`restore_windows = true`), then a fresh
/// `DriftWm` materializes them in z-order at their exact rects with `Quit`
/// origin.
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

        f.state().session_store_write_now();
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

        f.state().session_store_write_now();
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

        f.state().session_store_write_now();
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
        // This boot ends with its stand-in still on the canvas, as a real one
        // does, so it never reaches the teardown baseline.
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
        f.state().session_store_write_now();
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

    // Dismissing the explicit stand-in arms the debounce; its flush rewrites.
    let restored = suspended_in_order(&mut f);
    assert_eq!(restored.len(), 1);
    assert_eq!(restored[0].0.identity.app_id, "keepme");
    f.state().dismiss_suspended(restored[0].0.id);
    f.state().session_store_write_now();

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

    // Dismissing it arms the debounce; the flush it queued rewrites the file,
    // and the carried quit entry survives that rewrite.
    f.state().dismiss_suspended(restored[0].0.id);
    f.state().session_store_write_now();
    let after = session::read(&path);
    assert_eq!(after.entries.len(), 1);
    assert_eq!(after.entries[0].app_id, "onlyquit");
    assert_eq!(after.entries[0].origin, Origin::Quit);
}

/// Restore flipped on after a flag-off boot must not duplicate a relaunched app:
/// the carried-forward quit entry is dropped on the next flush (the live canvas
/// is authoritative), so the app serializes once, not twice.
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

    // Config hot-reload flips restore on; the next flush records the live windows.
    f.state().config.session.restore_windows = true;
    f.state().session_store_write_now();

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
    f.state().session_store_write_now();

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
    f.state().session_store_write_now();
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
    f.state().session_store_write_now();
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

/// Move `output`'s camera by `(dx, dy)` the way an interactive pan grab does:
/// straight into `output_state`, the route `set_camera_on`'s own doc records as
/// bypassing it.
fn pan_output(output: &smithay::output::Output, dx: f64, dy: f64) {
    let mut os = crate::state::output_state(output);
    os.camera += Point::from((dx, dy));
}

/// A pan arms the debounced write, so a session where the user only moved the
/// viewport still persists it; a sub-pixel nudge is float jitter and must not.
/// Each half flushes first — `dirty` is set by plenty of unrelated paths and
/// cleared only by a flush, so an unflushed "assert dirty" would pass with no
/// watcher at all.
#[test]
fn a_pan_arms_the_debounce_and_a_jitter_nudge_does_not() {
    let tmp = TempDir::new();
    let mut f = Fixture::new();
    let output = f.add_output(1, (1920, 1080));
    f.state().session_store.path = Some(tmp.path().join("session.json"));
    f.pump(1);

    f.state().session_store_write_now();
    pan_output(&output, 40.0, -25.0);
    f.pump(1);
    assert!(
        f.state().session_store_dirty(),
        "a pan is durable session state: the envelope always serializes cameras"
    );

    f.state().session_store_write_now();
    pan_output(&output, 0.4, 0.4);
    f.pump(1);
    assert!(
        !f.state().session_store_dirty(),
        "a sub-threshold nudge is jitter — arming on it would rewrite the file \
         once a second on an idle desktop"
    );
}

/// The same pair for zoom, whose threshold is far finer than the camera's.
#[test]
fn a_zoom_change_arms_the_debounce_and_a_finer_one_does_not() {
    let tmp = TempDir::new();
    let mut f = Fixture::new();
    let output = f.add_output(1, (1920, 1080));
    f.state().session_store.path = Some(tmp.path().join("session.json"));
    f.pump(1);

    f.state().session_store_write_now();
    crate::state::output_state(&output).zoom = 0.998;
    f.pump(1);
    assert!(
        f.state().session_store_dirty(),
        "a zoom past the threshold arms the durable write"
    );

    f.state().session_store_write_now();
    crate::state::output_state(&output).zoom = 0.9975;
    f.pump(1);
    assert!(
        !f.state().session_store_dirty(),
        "a zoom delta under 0.001 is jitter"
    );
}

/// Sub-threshold steps are measured against the last *armed* camera, not the
/// previous tick's: a slow continuous pan accumulates into an arming delta
/// instead of creeping across the canvas unrecorded.
#[test]
fn sub_threshold_drift_accumulates_until_it_arms() {
    let tmp = TempDir::new();
    let mut f = Fixture::new();
    let output = f.add_output(1, (1920, 1080));
    f.state().session_store.path = Some(tmp.path().join("session.json"));
    f.pump(1);
    f.state().session_store_write_now();

    for step in 1..=2 {
        pan_output(&output, 0.2, 0.0);
        f.pump(1);
        assert!(
            !f.state().session_store_dirty(),
            "{step} steps of 0.2px is still under the 0.5px threshold"
        );
    }
    pan_output(&output, 0.2, 0.0);
    f.pump(1);
    assert!(
        f.state().session_store_dirty(),
        "0.6px of accumulated drift crosses the threshold"
    );

    // Cancels the debounce timer the drift armed; `debug_counters` has no entry
    // for event-loop timers, so the teardown baseline would not catch one.
    f.state().session_store_write_now();
}

/// Viewport motion coalesces on the longer interval. Panning is this canvas's
/// primary interaction, and at the window interval a sustained pan would rewrite
/// the file once a second for the whole gesture. The four interval scenarios
/// assert which side of `WRITE_DEBOUNCE` a debounce landed on rather than any
/// absolute value — the timer is a real calloop one with no injectable clock.
#[test]
fn a_pan_arms_the_long_camera_debounce() {
    let tmp = TempDir::new();
    let mut f = Fixture::new();
    let output = f.add_output(1, (1920, 1080));
    f.state().session_store.path = Some(tmp.path().join("session.json"));
    f.pump(1);

    // Flush first: plenty of paths arm the short interval and only a flush
    // disarms, so an unflushed reading would measure whatever was already
    // pending instead of the pan.
    f.state().session_store_write_now();
    pan_output(&output, 40.0, -25.0);
    f.pump(1);
    let remaining = f.state().session_store_debounce_remaining();
    assert!(
        remaining.is_some_and(|left| left > WRITE_DEBOUNCE),
        "a pan alone waits out the camera interval, not the window one"
    );

    f.state().session_store_write_now();
}

/// A window mutation keeps the short interval: it is a discrete change the user
/// expects to survive, where a camera is continuous and self-correcting.
#[test]
fn a_window_mutation_arms_the_short_debounce() {
    let tmp = TempDir::new();
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let sid = f.state().insert_suspended_for_test(
        1,
        Point::from((0, 0)),
        Size::from((300, 200)),
        "s1",
        "S1",
    );
    f.state().session_store.path = Some(tmp.path().join("session.json"));
    f.pump(1);
    f.state().session_store_write_now();

    f.state().dismiss_suspended(sid);
    let remaining = f.state().session_store_debounce_remaining();
    assert!(
        remaining.is_some_and(|left| left <= WRITE_DEBOUNCE),
        "losing a dismiss to a crash is losing the user's own action"
    );

    f.state().session_store_write_now();
}

/// A window mutation arriving mid-pan pulls the pending write back in: the
/// nearer deadline wins, so a discrete change is never held for the camera's
/// sake.
#[test]
fn a_window_mutation_shortens_a_pending_camera_debounce() {
    let tmp = TempDir::new();
    let mut f = Fixture::new();
    let output = f.add_output(1, (1920, 1080));
    let sid = f.state().insert_suspended_for_test(
        1,
        Point::from((0, 0)),
        Size::from((300, 200)),
        "s1",
        "S1",
    );
    f.state().session_store.path = Some(tmp.path().join("session.json"));
    f.pump(1);
    f.state().session_store_write_now();

    pan_output(&output, 40.0, -25.0);
    f.pump(1);
    let after_the_pan = f.state().session_store_debounce_remaining();
    assert!(
        after_the_pan.is_some_and(|left| left > WRITE_DEBOUNCE),
        "precondition: the pan armed the camera interval"
    );

    f.state().dismiss_suspended(sid);
    let remaining = f.state().session_store_debounce_remaining();
    assert!(
        remaining.is_some_and(|left| left <= WRITE_DEBOUNCE),
        "the dismiss re-armed the pending write at the window interval"
    );

    f.state().session_store_write_now();
}

/// Not the other way round: a camera move while a window mutation is pending
/// leaves that deadline where it is instead of pushing the write out to the
/// camera interval — otherwise a pan started right after a dismiss would delay
/// it by four seconds.
#[test]
fn a_pan_does_not_extend_a_pending_window_debounce() {
    let tmp = TempDir::new();
    let mut f = Fixture::new();
    let output = f.add_output(1, (1920, 1080));
    let sid = f.state().insert_suspended_for_test(
        1,
        Point::from((0, 0)),
        Size::from((300, 200)),
        "s1",
        "S1",
    );
    f.state().session_store.path = Some(tmp.path().join("session.json"));
    f.pump(1);
    f.state().session_store_write_now();

    f.state().dismiss_suspended(sid);
    let after_the_dismiss = f.state().session_store_debounce_remaining();
    assert!(
        after_the_dismiss.is_some_and(|left| left <= WRITE_DEBOUNCE),
        "precondition: the dismiss armed the window interval"
    );

    pan_output(&output, 400.0, 300.0);
    f.pump(1);
    let remaining = f.state().session_store_debounce_remaining();
    assert!(
        remaining.is_some_and(|left| left <= WRITE_DEBOUNCE),
        "the pan rides the pending flush rather than deferring it"
    );

    f.state().session_store_write_now();
}

/// A connecting output only seeds the watcher's baseline — it does not arm.
/// Otherwise every boot would leave a pending debounce behind the outputs it
/// came up with.
#[test]
fn a_connecting_output_seeds_the_watcher_without_arming() {
    let tmp = TempDir::new();
    let mut f = Fixture::new();
    f.state().session_store.path = Some(tmp.path().join("session.json"));
    f.state().session_store_write_now();

    f.add_output(1, (1920, 1080));
    f.pump(1);
    assert!(
        !f.state().session_store_dirty(),
        "the output's camera is a first sight, not motion"
    );
}

/// With persistence off, a pan must not leave a debounce armed for a write that
/// can never happen.
#[test]
fn no_session_path_never_arms_on_a_pan() {
    let mut f = Fixture::new();
    let output = f.add_output(1, (1920, 1080));
    assert!(f.state().session_store.path.is_none());

    pan_output(&output, 500.0, 500.0);
    f.pump(1);
    assert!(
        !f.state().session_store_dirty(),
        "persistence is off entirely — a pan must not arm a timer that can never write"
    );
}

/// The write side records cameras unconditionally: a session that only panned,
/// with `restore_camera` off and no window ever touched, still has its viewport
/// in the file. What the watcher exists to get written.
#[test]
fn a_pan_alone_reaches_the_file() {
    let tmp = TempDir::new();
    let path = tmp.path().join("session.json");

    let mut f = Fixture::with_config(Config::default());
    assert!(!f.state().config.session.restore_camera);
    let output = f.add_output(1, (1920, 1080));
    f.state().session_store.path = Some(path.clone());
    f.pump(1);

    f.state().session_store_write_now();
    pan_output(&output, 400.0, 300.0);
    f.pump(1);
    assert!(f.state().session_store_dirty(), "the pan armed the write");
    // The debounce is a real 1s calloop timer with no injectable clock, so drive
    // the flush it would run rather than waiting it out.
    f.state().session_store_write_now();

    let saved = session::read(&path);
    let saved_output = saved
        .outputs
        .get("HEADLESS-1")
        .expect("the panned output is in the envelope");
    assert_eq!(saved_output.camera, [-560.0, -240.0]);
    assert!(saved.entries.is_empty(), "no window was ever touched");
}

/// Multi-output: a pan on the second monitor arms, and unplugging it drops its
/// baseline rather than arming — so a replug seeds afresh instead of diffing
/// the new camera against one from before the unplug.
#[test]
fn a_second_output_arms_on_its_own_pan_and_re_seeds_after_a_replug() {
    let tmp = TempDir::new();
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let second = f.add_output(2, (1920, 1080));
    f.state().session_store.path = Some(tmp.path().join("session.json"));
    f.pump(1);

    f.state().session_store_write_now();
    pan_output(&second, 40.0, -25.0);
    f.pump(1);
    assert!(
        f.state().session_store_dirty(),
        "the envelope carries every output's camera, not just the active one's"
    );

    f.state().session_store_write_now();
    f.remove_output(&second);
    assert!(
        !f.state().session_store_dirty(),
        "a disconnect is not viewport motion, and the focus hand-over that \
         comes with one — which arms the write on its own — has no window to \
         hand focus to here"
    );

    let second = f.add_output(2, (1920, 1080));
    f.pump(1);
    assert!(
        !f.state().session_store_dirty(),
        "the replugged output is a first sight again — its default camera is \
         not diffed against the one it had before the unplug"
    );

    f.state().session_store_write_now();
    pan_output(&second, 40.0, -25.0);
    f.pump(1);
    assert!(
        f.state().session_store_dirty(),
        "and it is watched from there on"
    );

    f.state().session_store_write_now();
}

/// Moving the focus arms the debounce, so the restored focus is as of the last
/// focus change rather than as of the last window the user happened to move.
/// Re-focusing what is already focused must not: `raise_and_focus` re-seats the
/// same intent on every click, and without the guard a user clicking one window
/// would rewrite the file once a second forever.
#[test]
fn focusing_another_window_arms_the_debounce_and_re_focusing_does_not() {
    let cache = TempDir::new();
    let tmp = TempDir::new();
    let mut f = Fixture::with_config(config_restore(true));
    f.add_output(1, (1920, 1080));
    inject_cache(&mut f, &cache, &["alpha", "beta"]);

    let a = f.add_client();
    map_at(&mut f, a, "alpha", (400, 300), (-500, -200));
    let b = f.add_client();
    map_at(&mut f, b, "beta", (200, 200), (100, -200));
    let alpha = window_by_app_id(&mut f, "alpha").unwrap();

    // Path last, then a pump, so the watcher seeds its baseline from wherever
    // placement left the camera and only the focus change is left to measure.
    f.state().session_store.path = Some(tmp.path().join("session.json"));
    f.pump(1);

    f.state().session_store_write_now();
    let serial = SERIAL_COUNTER.next_serial();
    f.state().raise_and_focus(&alpha, serial);
    f.pump(1);
    assert!(
        f.state().session_store_dirty(),
        "the envelope flags which entry held the focus — a focus change is a \
         durable change"
    );

    f.state().session_store_write_now();
    let serial = SERIAL_COUNTER.next_serial();
    f.state().raise_and_focus(&alpha, serial);
    f.pump(1);
    assert!(
        !f.state().session_store_dirty(),
        "the intent did not change, so there is nothing new to write"
    );
}

/// The stand-in half of the same contract: focus on a suspended window is
/// recorded too, and it moves through its own setter.
#[test]
fn focusing_a_stand_in_arms_the_debounce_and_re_focusing_does_not() {
    let tmp = TempDir::new();
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let first = f.state().insert_suspended_for_test(
        1,
        Point::from((0, 0)),
        Size::from((300, 200)),
        "s1",
        "S1",
    );
    let second = f.state().insert_suspended_for_test(
        2,
        Point::from((500, 0)),
        Size::from((300, 200)),
        "s2",
        "S2",
    );
    f.state().focus_and_raise_suspended(first);

    f.state().session_store.path = Some(tmp.path().join("session.json"));
    f.pump(1);

    f.state().session_store_write_now();
    f.state().focus_and_raise_suspended(second);
    f.pump(1);
    assert!(
        f.state().session_store_dirty(),
        "focus moved to the other stand-in"
    );

    f.state().session_store_write_now();
    f.state().focus_and_raise_suspended(second);
    f.pump(1);
    assert!(
        !f.state().session_store_dirty(),
        "the intent did not change, so there is nothing new to write"
    );

    f.state().dismiss_suspended(first);
    f.state().dismiss_suspended(second);
    // Cancels the debounce the dismissals armed; the teardown baseline covers
    // the stage, not the event loop's timers.
    f.state().session_store_write_now();
}

/// The third write site: `focus_changed` rewrites the intent for focus seated
/// on the keyboard directly, which is the only route the dead-intent history
/// recovery takes. The launcher shape reaches it — an exclusive layer holds the
/// seat focus while the intended window dies, so the destroy's focus-follow
/// (which wants the seat focus or the history head) never fires and leaves the
/// intent pointing at a dead surface; the recovery to the survivor then happens
/// inside the layer's teardown, with no setter involved.
#[test]
fn the_history_recovery_after_a_layer_teardown_arms_the_debounce() {
    use smithay::utils::IsAlive;

    let tmp = TempDir::new();
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    map_at(&mut f, id, "survivor", (400, 300), (-500, -200));
    let doomed_surface = map_at(&mut f, id, "doomed", (200, 200), (100, -200));
    let survivor = window_by_app_id(&mut f, "survivor").unwrap();
    let doomed = window_by_app_id(&mut f, "doomed").unwrap();

    // The last real focus change is on the survivor, so it — not the window
    // about to die — is the history head.
    let serial = SERIAL_COUNTER.next_serial();
    f.state().raise_and_focus(&survivor, serial);

    let launcher = f
        .client(id)
        .create_layer(None, zwlr_layer_shell_v1::Layer::Overlay, "launcher");
    let ls = launcher.surface.clone();
    launcher.set_configure_props(super::client::LayerConfigureProps {
        size: Some((400, 300)),
        kb_interactivity: Some(zwlr_layer_surface_v1::KeyboardInteractivity::Exclusive),
        ..Default::default()
    });
    launcher.commit();
    f.roundtrip(id);
    let layer = f.client(id).layer(&ls);
    layer.set_size(400, 300);
    layer.attach_new_buffer();
    layer.ack_last_and_commit();
    f.double_roundtrip(id);

    // Hover under the launcher moves the intent while the layer keeps the seat
    // focus — the case the setter's doc comment is written for.
    let serial = SERIAL_COUNTER.next_serial();
    f.state()
        .set_window_focus(Some(FocusTarget(server_surface(&doomed))), serial);
    f.client(id).window(&doomed_surface).destroy();
    f.double_roundtrip(id);
    assert!(
        f.state()
            .window_focus_surface()
            .is_some_and(|t| !t.0.alive()),
        "the intent is left pointing at the window that died under the launcher"
    );

    f.state().session_store.path = Some(tmp.path().join("session.json"));
    f.pump(1);
    f.state().session_store_write_now();

    f.client(id).layer(&ls).layer_surface.destroy();
    f.double_roundtrip(id);
    assert_eq!(
        f.state().focused_window().as_ref(),
        Some(&survivor),
        "the recovery landed on the surviving window"
    );
    assert!(
        f.state().session_store_dirty(),
        "the recovery's rewrite is a focus change like any other"
    );

    f.client(id).layer(&ls).surface.destroy();
    f.double_roundtrip(id);
    f.state().session_store_write_now();
}

/// A create and a dismiss only *arm* the debounce — neither rebuilds the
/// envelope from inside its handler, because the conversion runs from
/// `toplevel_destroyed`, which a logout reaches with the stage already
/// draining. The flush the timer would run is what puts either change in the
/// file. Drives the real conversion path, not the test-only insertion hook.
#[test]
fn create_and_dismiss_arm_the_debounce() {
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

    // Mapping the window already set the flag, and only a flush clears it.
    f.state().session_store_write_now();

    // Suspend → convert.
    f.state()
        .execute_action(&driftwm::config::Action::SuspendWindow);
    f.client(id).window(&surface).destroy();
    f.roundtrip(id);
    f.dispatch();
    assert!(
        f.state().session_store_dirty(),
        "the conversion armed the debounce rather than writing from the \
         destroy handler"
    );

    // What that debounce writes when it comes due.
    f.state().session_store_write_now();
    let after_create = session::read(&path);
    assert_eq!(after_create.entries.len(), 1);
    assert_eq!(after_create.entries[0].app_id, "myapp");
    assert_eq!(after_create.entries[0].origin, Origin::Explicit);
    assert!(
        after_create.entries[0].focused,
        "the stand-in inherited the closed window's focus, and the flush kept it"
    );

    let sid = after_create.entries[0].id;
    f.state().dismiss_suspended(crate::state::SuspendedId(sid));
    assert!(f.state().session_store_dirty(), "the dismiss armed it too");

    f.state().session_store_write_now();
    assert!(
        session::read(&path).entries.is_empty(),
        "and that flush drops the dismissed stand-in"
    );
}

/// A steady-state flush records live windows, not just suspended stand-ins:
/// the crash-recovery contract, and — since nothing writes at shutdown — the
/// only thing a logout leaves behind. A SIGKILL lands with the canvas as of the
/// last flush in the file instead of an empty rewrite that erases the prior
/// session.
#[test]
fn steady_state_flush_writes_live_windows() {
    let cache = TempDir::new();
    let tmp = TempDir::new();
    let path = tmp.path().join("session.json");

    let mut f = Fixture::with_config(config_restore(true));
    f.add_output(1, (1920, 1080));
    inject_cache(&mut f, &cache, &["alpha", "beta"]);
    f.state().session_store.path = Some(path.clone());

    let a = f.add_client();
    map_at(&mut f, a, "alpha", (400, 300), (500, 500));
    let b = f.add_client();
    map_at(&mut f, b, "beta", (200, 200), (-300, 100));

    f.state().session_store_write_now();

    // Both live windows, in z-order, as quit records at their shrunken-body
    // rects.
    let saved = session::read(&path);
    assert_eq!(saved.entries.len(), 2);
    assert_eq!(saved.entries[0].app_id, "alpha");
    assert_eq!(saved.entries[0].position, [700, -650]);
    assert_eq!(saved.entries[0].size, [400, 300]);
    assert_eq!(saved.entries[1].app_id, "beta");
    assert_eq!(saved.entries[1].position, [-200, -200]);
    assert_eq!(saved.entries[1].size, [200, 200]);
    assert!(saved.entries.iter().all(|e| e.origin == Origin::Quit));

    // The flush is a true steady-state write: the file loads back into a fresh
    // compositor, so a crash mid-session recovers the canvas.
    let mut f = Fixture::with_config(config_restore(true));
    f.add_output(1, (1920, 1080));
    f.state().session_store.path = Some(path.clone());
    f.state().load_session();
    let restored = suspended_in_order(&mut f);
    assert_eq!(restored.len(), 2);
    assert_eq!(restored[0].0.identity.app_id, "alpha");
    assert_eq!(restored[0].1, Point::from((500, 525)));
    for (s, _) in restored {
        f.state().dismiss_suspended(s.id);
    }
}

/// Closing a live window drops it from the steady-state file: the teardown
/// arms the same debounce as a create, so a crash after a manual close cannot
/// resurrect a window the user already dismissed.
#[test]
fn closing_a_live_window_drops_it_from_the_steady_state_file() {
    let cache = TempDir::new();
    let tmp = TempDir::new();
    let path = tmp.path().join("session.json");

    let mut f = Fixture::with_config(config_restore(true));
    f.add_output(1, (1920, 1080));
    inject_cache(&mut f, &cache, &["alpha", "beta"]);
    f.state().session_store.path = Some(path.clone());

    let a = f.add_client();
    map_at(&mut f, a, "alpha", (400, 300), (500, 500));
    let b = f.add_client();
    let b_surface = map_at(&mut f, b, "beta", (200, 200), (-300, 100));
    f.state().session_store_write_now();
    assert_eq!(session::read(&path).entries.len(), 2);

    // The user closes beta; the client teardown unmaps it and arms the debounce.
    f.client(b).window(&b_surface).destroy();
    f.roundtrip(b);
    f.dispatch();
    f.state().session_store_write_now();

    let saved = session::read(&path);
    assert_eq!(saved.entries.len(), 1);
    assert_eq!(saved.entries[0].app_id, "alpha");
    assert_eq!(saved.entries[0].position, [700, -650]);
}

/// A logout SIGTERMs the clients and the compositor together, so client
/// teardown runs while the compositor is still up: it must leave the file
/// exactly as the last flush wrote it, killed windows included.
///
/// The dirty flag is the load-bearing half. The content on its own proves
/// nothing — the fixture cannot re-run a write at process exit, which is the
/// thing being deleted — but any synchronous rebuild reached from the teardown
/// would both clear the flag and write the drained stage, so the pair says the
/// file is still the earlier write and the teardown only armed the debounce.
#[test]
fn a_teardown_that_kills_clients_leaves_the_saved_session_intact() {
    let cache = TempDir::new();
    let tmp = TempDir::new();
    let path = tmp.path().join("session.json");

    let config = Config::from_toml(
        "[session]\nrestore_windows = true\nrestore_bookmarks = true\n\
         [decorations]\ndefault_mode = \"server\"\n",
    )
    .unwrap();
    let mut f = Fixture::with_config(config);
    let output = f.add_output(1, (1920, 1080));
    // Without a resolvable identity for every app, `live_window_entry` records
    // nothing and every assertion below passes against an empty file.
    inject_cache(&mut f, &cache, &["alpha", "beta", "gamma"]);
    f.state().session_store.path = Some(path.clone());
    f.state().bookmarks.insert("desk".into(), [12.0, -34.0]);

    let a = f.add_client();
    map_at(&mut f, a, "alpha", (400, 300), (500, 500));
    let b = f.add_client();
    map_at(&mut f, b, "beta", (200, 200), (-300, 100));
    let c = f.add_client();
    map_at(&mut f, c, "gamma", (300, 300), (900, -400));
    pan_output(&output, 120.0, -80.0);
    f.pump(1);

    // The last durable write before the logout.
    f.state().session_store_write_now();
    let before = session::read(&path);
    assert_eq!(before.entries.len(), 3, "precondition: all three are saved");
    let camera = crate::state::output_state(&output).camera;

    // Two of the three clients die while the compositor keeps running, as a
    // logout's SIGTERM makes them.
    f.kill_client(a);
    f.kill_client(b);
    f.pump(10);
    assert_eq!(
        f.state().stage.windows().count(),
        1,
        "precondition: the two disconnects drained the stage down to gamma"
    );

    let after = session::read(&path);
    assert_eq!(
        after.entries, before.entries,
        "the teardown wrote nothing: all three windows are still in the file"
    );
    assert!(
        f.state().session_store_dirty(),
        "it armed the debounce instead — a synchronous rebuild would have \
         cleared this and dropped the two killed windows"
    );
    assert_eq!(
        after.outputs["HEADLESS-1"].camera,
        [camera.x, camera.y],
        "the rest of the envelope is untouched too"
    );
    assert_eq!(after.bookmarks["desk"], [12.0, -34.0]);

    // Cancels the debounce the teardown armed; `debug_counters` has no entry
    // for event-loop timers, so the teardown baseline would not catch one.
    f.state().session_store_write_now();
}

/// The same race under `suspend_on_close`, which the default config cannot
/// catch: the conversion runs from `toplevel_destroyed` and cannot tell a
/// user's close from a logout killing the client, so a synchronous rebuild
/// there rewrites the file from whatever is left on the stage.
///
/// One per-app rule is enough to reach it, so that is what this uses: alpha's
/// disconnect really does leave the stage, beta's converts. The two kills are
/// sequenced rather than batched because a real logout's dispatch order is
/// epoll's — this pins the order that loses data, instead of hoping for it.
#[test]
fn a_suspend_on_close_conversion_during_teardown_does_not_rewrite_the_file() {
    let cache = TempDir::new();
    let tmp = TempDir::new();
    let path = tmp.path().join("session.json");

    let config = config_restore_with_rule(
        true,
        "[[window_rules]]\napp_id = \"beta\"\nsuspend_on_close = true\n",
    );
    let mut f = Fixture::with_config(config);
    f.add_output(1, (1920, 1080));
    inject_cache(&mut f, &cache, &["alpha", "beta", "gamma"]);
    f.state().session_store.path = Some(path.clone());

    let a = f.add_client();
    map_at(&mut f, a, "alpha", (400, 300), (500, 500));
    let b = f.add_client();
    map_at(&mut f, b, "beta", (200, 200), (-300, 100));
    let c = f.add_client();
    map_at(&mut f, c, "gamma", (300, 300), (900, -400));

    f.state().session_store_write_now();
    let before = session::read(&path);
    assert_eq!(before.entries.len(), 3, "precondition: all three are saved");

    // alpha leaves the stage first…
    f.kill_client(a);
    f.pump(10);
    assert_eq!(
        f.state().stage.windows().count(),
        2,
        "precondition: the unruled app really closed instead of converting"
    );

    // …then beta's disconnect converts it, from a stage alpha has already left.
    f.kill_client(b);
    f.pump(10);
    let sid = suspended_in_order(&mut f)
        .first()
        .map(|(s, _)| s.id)
        .expect("precondition: beta converted to a stand-in");

    let after = session::read(&path);
    assert_eq!(
        after.entries, before.entries,
        "the conversion armed the debounce instead of rebuilding the envelope \
         from the drained stage, which would have lost alpha"
    );
    assert!(f.state().session_store_dirty());

    f.state().dismiss_suspended(sid);
    f.state().session_store_write_now();
}

/// A debounce armed just before a client dies flushes the stage as it is when
/// the timer comes due, not as it was when the move armed it — the kill variant
/// of the graceful-close case above.
#[test]
fn a_debounce_armed_before_a_kill_flushes_without_the_killed_window() {
    let cache = TempDir::new();
    let tmp = TempDir::new();
    let path = tmp.path().join("session.json");

    let mut f = Fixture::with_config(config_restore(true));
    f.add_output(1, (1920, 1080));
    inject_cache(&mut f, &cache, &["alpha", "beta"]);
    f.state().session_store.path = Some(path.clone());

    let a = f.add_client();
    map_at(&mut f, a, "alpha", (400, 300), (500, 500));
    let b = f.add_client();
    map_at(&mut f, b, "beta", (200, 200), (-300, 100));
    f.state().session_store_write_now();

    // The user drags beta, arming the debounce; then its client is killed
    // before the second is up.
    let beta = window_by_app_id(&mut f, "beta").unwrap();
    let serial = SERIAL_COUNTER.next_serial();
    f.state().raise_and_focus(&beta, serial);
    f.state()
        .execute_action(&driftwm::config::Action::NudgeWindow(
            driftwm::config::Direction::Right,
        ));
    assert!(
        f.state().session_store_dirty(),
        "precondition: the nudge armed the debounce"
    );

    f.kill_client(b);
    f.pump(10);
    f.state().session_store_write_now();

    let saved = session::read(&path);
    assert_eq!(
        saved.entries.len(),
        1,
        "the pending write records the stage it flushes from, not the one that \
         armed it"
    );
    assert_eq!(saved.entries[0].app_id, "alpha");
    assert_eq!(saved.entries[0].position, [700, -650]);
}

/// The IPC `resize` verb (and the grow/shrink steps behind it) settles on the
/// client's answering commit, which arms the session-store debounce like a grab
/// resize's settle does — a crash after `msg resize` restores the new size, not
/// the pre-resize one.
#[test]
fn ipc_resize_of_a_live_window_persists_at_steady_state() {
    let cache = TempDir::new();
    let tmp = TempDir::new();
    let path = tmp.path().join("session.json");

    let mut f = Fixture::with_config(config_restore(true));
    f.add_output(1, (1920, 1080));
    inject_cache(&mut f, &cache, &["alpha"]);
    f.state().session_store.path = Some(path.clone());

    let id = f.add_client();
    let surface = map_at(&mut f, id, "alpha", (400, 300), (500, 500));
    f.state().session_store_write_now();
    assert_eq!(session::read(&path).entries[0].size, [400, 300]);

    assert_eq!(
        crate::ipc::dispatch(
            crate::ipc::protocol::Request::Resize {
                window: None,
                to: Some((600, 500)),
            },
            f.state(),
        ),
        Ok(crate::ipc::protocol::Response::Size {
            width: 600,
            height: 500,
        })
    );
    // Deliver the configure the request queued before adopting it, or the
    // client takes the stale map-time configure (an empty size) for the new one.
    f.roundtrip(id);
    super::adopt_last_configure(&mut f, id, &surface);
    f.dispatch();

    // The answering commit armed the debounce; flush straight through and the
    // new size is what a crash would restore.
    f.state().session_store_write_now();
    let saved = session::read(&path);
    assert_eq!(saved.entries.len(), 1);
    assert_eq!(saved.entries[0].size, [600, 500]);
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
        // The conversion only armed the debounce; run the flush it queued.
        f.state().session_store_write_now();

        // Tear the stand-in down cleanly for the fixture baseline, but keep the
        // durable file (clear the path so the dismiss's debounce can't rewrite
        // it empty).
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
    // No path injected: the flush is a no-op that touches no file, and the arm
    // neither sets the flag nor registers a debounce timer — which nothing else
    // would catch, since `debug_counters` tracks neither of those.
    f.state().session_store.path = None;
    f.state().session_store_write_now();
    f.state().session_store_mark_dirty();
    assert!(
        !f.state().session_store_dirty(),
        "an arm with nowhere to write must not queue one"
    );
}

/// A `restore_windows = false` rule keeps its app's live window out of the
/// durable save even with the global flag on, while an unruled app's live
/// window still saves — proving the exclusion is the rule, not a missing
/// desktop entry or some other blanket ineligibility.
#[test]
fn restore_windows_false_rule_excludes_matching_app_from_the_durable_save() {
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

    f.state().session_store_write_now();

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
    // it out of the durable save — and the record is carried forward once, not
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

    f.state().session_store_write_now();
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

/// An explicitly suspended stand-in is saved even for an app a
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

    f.state().session_store_write_now();

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
/// next durable save without either window remapping.
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

    f.state().session_store_write_now();

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

    f.state().session_store_write_now();

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

        f.state().session_store_write_now();

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
