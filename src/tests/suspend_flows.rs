//! Suspend-flow conformance (§8): the `suspend-window` action, the
//! `suspend_on_close` conversion path, the mark lifecycle (with an injected
//! clock), the close/dismiss semantics on a suspended window, and the IPC
//! inventory. Unlike `suspended.rs`, these drive the *production* conversion
//! (`toplevel_destroyed`), not the test-only insertion hook.

use std::time::{Duration, Instant};

use driftwm::config::{Action, Config};
use driftwm::desktop_entry::DesktopEntryCache;
use driftwm::layout::snap::SnapRect;
use smithay::reexports::wayland_server::Resource;
use smithay::utils::{Point, SERIAL_COUNTER, Size};

use crate::ipc::protocol::{Request, Response, WindowSelector};
use crate::state::{StageWindow, SuspendedId};

use super::real::TempDir;
use super::{Fixture, map_window, server_surface, window_by_app_id};

/// SSD-on config plus an optional top-level / per-rule `suspend_on_close` body.
fn config_with(body: &str) -> Config {
    Config::from_toml(&format!(
        "{body}\n[decorations]\ndefault_mode = \"server\"\n"
    ))
    .unwrap()
}

fn origin_view(f: &mut Fixture) {
    f.state().set_camera(Point::from((0.0, 0.0)));
    f.state().with_output_state(|os| {
        os.zoom = 1.0;
        os.camera = Point::from((0.0, 0.0));
    });
}

/// Write `{stem}.desktop` files into `tmp` and seat them as the compositor's
/// desktop-entry cache, so those app_ids resolve to a launchable identity.
fn inject_cache(f: &mut Fixture, tmp: &TempDir, stems: &[&str]) {
    for stem in stems {
        let contents = format!("[Desktop Entry]\nType=Application\nName={stem}\nExec={stem}\n");
        std::fs::write(tmp.path().join(format!("{stem}.desktop")), contents).unwrap();
    }
    f.state().desktop_entry_cache = Some(DesktopEntryCache::new(vec![tmp.path().to_path_buf()]));
}

/// Map a client at `app_id`/`size` and park it at a known canvas position.
/// Returns the client-side surface (to destroy later) and the server window.
fn map_at(
    f: &mut Fixture,
    id: super::client::ClientId,
    app_id: &str,
    size: (u16, u16),
    pos: (i32, i32),
) -> (
    wayland_client::protocol::wl_surface::WlSurface,
    smithay::desktop::Window,
) {
    let surface = map_window(f, id, app_id, size);
    let window = window_by_app_id(f, app_id).unwrap();
    f.state()
        .map_window(StageWindow::Client(window.clone()), Point::from(pos), true);
    (surface, window)
}

/// Client-initiated clean close, driven to the server.
fn client_close(
    f: &mut Fixture,
    id: super::client::ClientId,
    surface: &wayland_client::protocol::wl_surface::WlSurface,
) {
    f.client(id).window(surface).destroy();
    f.roundtrip(id);
    f.dispatch();
}

/// The lone suspended stand-in on the stage, if any.
fn suspended_id(f: &mut Fixture) -> Option<SuspendedId> {
    f.state()
        .stage
        .windows()
        .find_map(|w| w.suspended().map(|s| s.id))
}

/// The `suspend-window` action converts the focused window into a stand-in in
/// place: same canvas rect, same z-slot, same `ElementId`, and the keyboard
/// focus intent moves onto the stand-in.
#[test]
fn explicit_suspend_converts_in_place() {
    let tmp = TempDir::new();
    let mut f = Fixture::with_config(config_with(""));
    f.add_output(1, (1920, 1080));
    inject_cache(&mut f, &tmp, &["myapp"]);
    let id = f.add_client();
    let (surface, target) = map_at(&mut f, id, "myapp", (400, 300), (500, 500));
    origin_view(&mut f);

    // A second window on top so the target sits at z-slot 0 (not topmost).
    let id2 = f.add_client();
    let _top = map_window(&mut f, id2, "other", (200, 200));

    let eid = f.state().stage.id_of(&target).unwrap();
    let idx = f
        .state()
        .stage
        .windows()
        .position(|w| *w == target)
        .unwrap();

    // Focus the target without raising it, so the z-slot check is meaningful.
    let focus = server_surface(&target);
    let serial = SERIAL_COUNTER.next_serial();
    f.state()
        .set_window_focus(Some(crate::state::FocusTarget(focus)), serial);

    f.state().execute_action(&Action::SuspendWindow);
    // The action recorded a mark and asked the client to close.
    assert_eq!(f.state().debug_counters()["suspend_marks"], 1);
    client_close(&mut f, id, &surface);

    let sid = suspended_id(&mut f).expect("a suspended stand-in replaced the client");
    // Same slot + id.
    assert_eq!(
        f.state()
            .stage
            .windows()
            .position(|w| w.suspended().is_some()),
        Some(idx),
        "the stand-in kept the client's z-slot"
    );
    let elem = StageWindow::Suspended(f.state().find_suspended(sid).unwrap());
    assert_eq!(
        f.state().stage.id_of(&elem),
        Some(eid),
        "ElementId preserved"
    );
    // Same rect.
    let s = f.state().find_suspended(sid).unwrap();
    assert_eq!(
        f.state().stage.position_of(&elem),
        Some(Point::from((500, 500)))
    );
    assert_eq!(s.size.get(), Size::from((400, 300)));
    // Focus intent moved onto the stand-in; the mark was consumed.
    assert_eq!(f.state().gated_suspended_focus(), Some(sid));
    assert_eq!(f.state().debug_counters()["suspend_marks"], 0);

    f.state().dismiss_suspended(sid);
    close_client(&mut f, id2);
}

/// Firing `suspend-window` again on a focused suspended window dismisses the
/// stand-in — the put-away gesture escalates, like a second close-button click.
#[test]
fn explicit_suspend_repeated_dismisses_the_stand_in() {
    let tmp = TempDir::new();
    let mut f = Fixture::with_config(config_with(""));
    f.add_output(1, (1920, 1080));
    inject_cache(&mut f, &tmp, &["myapp"]);
    let id = f.add_client();
    let (surface, target) = map_at(&mut f, id, "myapp", (400, 300), (500, 500));
    origin_view(&mut f);
    let serial = SERIAL_COUNTER.next_serial();
    f.state().raise_and_focus(&target, serial);

    f.state().execute_action(&Action::SuspendWindow);
    client_close(&mut f, id, &surface);
    let sid = suspended_id(&mut f).expect("a suspended stand-in replaced the client");
    assert_eq!(f.state().gated_suspended_focus(), Some(sid));

    f.state().execute_action(&Action::SuspendWindow);
    assert!(
        f.state().find_suspended(sid).is_none(),
        "the second press dismissed the stand-in"
    );
    assert_eq!(f.state().stage.windows().count(), 0);
    assert_eq!(
        f.state().debug_counters()["suspend_marks"],
        0,
        "no new mark planted by the dismissing press"
    );
}

/// Tidy a leftover client so the fixture's baseline check passes.
fn close_client(f: &mut Fixture, id: super::client::ClientId) {
    let surface = f.client(id).state.windows[0].surface.clone();
    client_close(f, id, &surface);
}

/// Suspending a fullscreen window records the windowed (pre-fullscreen) rect,
/// not the fullscreen buffer the client still reports until it acks the exit.
#[test]
fn explicit_suspend_of_fullscreen_uses_windowed_rect() {
    let tmp = TempDir::new();
    let mut f = Fixture::with_config(config_with(""));
    f.add_output(1, (1920, 1080));
    inject_cache(&mut f, &tmp, &["myapp"]);
    let id = f.add_client();
    // No camera override: leaving the camera at its natural (output-aligned)
    // position keeps the fullscreen park a no-op, so the render-cache counters
    // return to baseline. This test only checks the stand-in's size.
    let (surface, target) = map_at(&mut f, id, "myapp", (400, 300), (200, 200));
    let serial = SERIAL_COUNTER.next_serial();
    f.state().raise_and_focus(&target, serial);

    // Enter fullscreen: geometry balloons to the output size (1920x1080).
    f.client(id).window(&surface).set_fullscreen(None);
    f.double_roundtrip(id);
    f.client(id).window(&surface).ack_last_and_commit();
    f.double_roundtrip(id);
    assert!(f.state().stage.has_fullscreen());

    f.state().execute_action(&Action::SuspendWindow);
    client_close(&mut f, id, &surface);

    let sid = suspended_id(&mut f).expect("fullscreen window suspended");
    assert_eq!(
        f.state().find_suspended(sid).unwrap().size.get(),
        Size::from((400, 300)),
        "the stand-in is the windowed size, not the fullscreen buffer"
    );
    assert!(
        !f.state().stage.has_fullscreen(),
        "fullscreen exited on suspend"
    );
    f.state().dismiss_suspended(sid);
}

/// A `suspend_on_close` (markless) conversion of a fullscreen self-close seats
/// the stand-in at the pre-fullscreen rect — position AND size — not the
/// fullscreen buffer parked at the camera origin.
#[test]
fn suspend_on_close_of_fullscreen_uses_windowed_rect() {
    let tmp = TempDir::new();
    let mut f = Fixture::with_config(config_with("suspend_on_close = true"));
    f.add_output(1, (1920, 1080));
    inject_cache(&mut f, &tmp, &["myapp"]);
    let id = f.add_client();
    // No camera override, so the fullscreen park is a no-op and the render
    // counters return to baseline (mirrors the explicit-suspend fullscreen test).
    let (surface, target) = map_at(&mut f, id, "myapp", (400, 300), (200, 200));
    let serial = SERIAL_COUNTER.next_serial();
    f.state().raise_and_focus(&target, serial);

    // Enter fullscreen: geometry balloons to the output size.
    f.client(id).window(&surface).set_fullscreen(None);
    f.double_roundtrip(id);
    f.client(id).window(&surface).ack_last_and_commit();
    f.double_roundtrip(id);
    assert!(f.state().stage.has_fullscreen());

    // The client self-closes while fullscreen — no suspend/close action, so the
    // markless suspend_on_close path runs.
    client_close(&mut f, id, &surface);

    let sid = suspended_id(&mut f).expect("fullscreen self-close converted under the flag");
    let s = f.state().find_suspended(sid).unwrap();
    assert_eq!(
        s.size.get(),
        Size::from((400, 300)),
        "the stand-in is the windowed size, not the fullscreen buffer"
    );
    let elem = StageWindow::Suspended(s);
    assert_eq!(
        f.state().stage.position_of(&elem),
        Some(Point::from((200, 200))),
        "the stand-in is at the pre-fullscreen position, not the camera park"
    );
    assert!(
        !f.state().stage.has_fullscreen(),
        "fullscreen state was torn down"
    );
    f.state().dismiss_suspended(sid);
}

/// A window with no `.desktop` entry can never relaunch, so `suspend-window`
/// falls back to a plain close (no stand-in, no mark left behind).
#[test]
fn suspend_without_desktop_entry_closes_plainly() {
    let tmp = TempDir::new();
    let mut f = Fixture::with_config(config_with(""));
    f.add_output(1, (1920, 1080));
    inject_cache(&mut f, &tmp, &["something-else"]);
    let id = f.add_client();
    let (surface, target) = map_at(&mut f, id, "unknown", (400, 300), (300, 300));
    origin_view(&mut f);
    let serial = SERIAL_COUNTER.next_serial();
    f.state().raise_and_focus(&target, serial);

    f.state().execute_action(&Action::SuspendWindow);
    assert_eq!(
        f.state().debug_counters()["suspend_marks"],
        0,
        "no mark for an unresolvable window"
    );
    client_close(&mut f, id, &surface);

    assert!(
        suspended_id(&mut f).is_none(),
        "no stand-in without an entry"
    );
    assert_eq!(f.state().stage.windows().count(), 0);
}

/// A refused close (the client survives) lets the suspend mark lapse: a later
/// close is a plain close. The 10 s deadline is driven by the injected clock.
#[test]
fn suspend_mark_expires_then_close_is_plain() {
    let tmp = TempDir::new();
    let mut f = Fixture::with_config(config_with(""));
    f.add_output(1, (1920, 1080));
    inject_cache(&mut f, &tmp, &["myapp"]);
    let id = f.add_client();
    let (surface, target) = map_at(&mut f, id, "myapp", (400, 300), (300, 300));
    origin_view(&mut f);
    let serial = SERIAL_COUNTER.next_serial();
    f.state().raise_and_focus(&target, serial);

    f.state().execute_action(&Action::SuspendWindow);
    assert_eq!(f.state().debug_counters()["suspend_marks"], 1);

    // The client refuses (dialog); the deadline passes.
    f.state()
        .sweep_marks(Instant::now() + Duration::from_secs(11));
    assert_eq!(
        f.state().debug_counters()["suspend_marks"],
        0,
        "mark lapsed"
    );

    // The eventual close is plain — no stand-in.
    client_close(&mut f, id, &surface);
    assert!(suspended_id(&mut f).is_none());
    assert_eq!(f.state().stage.windows().count(), 0);
}

/// Real-close marks share the same TTL: a refused `close-window` mark expires,
/// so a later client self-close still converts under `suspend_on_close`.
#[test]
fn real_close_mark_expires_allowing_conversion() {
    let tmp = TempDir::new();
    let mut f = Fixture::with_config(config_with("suspend_on_close = true"));
    f.add_output(1, (1920, 1080));
    inject_cache(&mut f, &tmp, &["myapp"]);
    let id = f.add_client();
    let (surface, target) = map_at(&mut f, id, "myapp", (400, 300), (300, 300));
    origin_view(&mut f);
    let serial = SERIAL_COUNTER.next_serial();
    f.state().raise_and_focus(&target, serial);

    // close-window sets a real-close mark; the client refuses to close.
    f.state().execute_action(&Action::CloseWindow);
    assert_eq!(f.state().debug_counters()["real_close_marks"], 1);

    f.state()
        .sweep_marks(Instant::now() + Duration::from_secs(11));
    assert_eq!(f.state().debug_counters()["real_close_marks"], 0);

    // A later client-initiated close now converts (the stale mark is gone).
    client_close(&mut f, id, &surface);
    assert!(
        suspended_id(&mut f).is_some(),
        "an expired real-close mark no longer forces a real close"
    );
    let sid = suspended_id(&mut f).unwrap();
    f.state().dismiss_suspended(sid);
}

/// With both marks live on a close-refusing window, the later command wins:
/// suspend-window escalated to close-window closes for real, and the reverse
/// order converts.
#[test]
fn later_mark_wins_when_both_are_live() {
    // suspend-window, refused, then close-window → real close.
    {
        let tmp = TempDir::new();
        let mut f = Fixture::with_config(config_with(""));
        f.add_output(1, (1920, 1080));
        inject_cache(&mut f, &tmp, &["myapp"]);
        let id = f.add_client();
        let (surface, target) = map_at(&mut f, id, "myapp", (400, 300), (300, 300));
        origin_view(&mut f);
        let serial = SERIAL_COUNTER.next_serial();
        f.state().raise_and_focus(&target, serial);

        f.state().execute_action(&Action::SuspendWindow);
        f.state().execute_action(&Action::CloseWindow);
        client_close(&mut f, id, &surface);
        assert!(
            suspended_id(&mut f).is_none(),
            "the later close-window wins over the refused suspend"
        );
        assert_eq!(f.state().stage.windows().count(), 0);
    }
    // close-window, refused, then suspend-window → converts.
    {
        let tmp = TempDir::new();
        let mut f = Fixture::with_config(config_with(""));
        f.add_output(1, (1920, 1080));
        inject_cache(&mut f, &tmp, &["myapp"]);
        let id = f.add_client();
        let (surface, target) = map_at(&mut f, id, "myapp", (400, 300), (300, 300));
        origin_view(&mut f);
        let serial = SERIAL_COUNTER.next_serial();
        f.state().raise_and_focus(&target, serial);

        f.state().execute_action(&Action::CloseWindow);
        f.state().execute_action(&Action::SuspendWindow);
        client_close(&mut f, id, &surface);
        let sid = suspended_id(&mut f).expect("the later suspend-window wins");
        f.state().dismiss_suspended(sid);
    }
}

/// `suspend_on_close` converts a client-initiated close (CSD X, in-app quit)
/// into a stand-in.
#[test]
fn suspend_on_close_client_self_close_converts() {
    let tmp = TempDir::new();
    let mut f = Fixture::with_config(config_with("suspend_on_close = true"));
    f.add_output(1, (1920, 1080));
    inject_cache(&mut f, &tmp, &["myapp"]);
    let id = f.add_client();
    let (surface, _target) = map_at(&mut f, id, "myapp", (400, 300), (200, 200));
    origin_view(&mut f);

    client_close(&mut f, id, &surface);
    let sid = suspended_id(&mut f).expect("client self-close converted under the flag");
    f.state().dismiss_suspended(sid);
}

/// The compositor-driven escape hatches — `close-window`, `msg close`, and
/// taskbar (foreign-toplevel) close — stay real closes even under the flag.
#[test]
fn suspend_on_close_compositor_closes_do_not_convert() {
    // close-window action.
    {
        let tmp = TempDir::new();
        let mut f = Fixture::with_config(config_with("suspend_on_close = true"));
        f.add_output(1, (1920, 1080));
        inject_cache(&mut f, &tmp, &["myapp"]);
        let id = f.add_client();
        let (surface, target) = map_at(&mut f, id, "myapp", (400, 300), (200, 200));
        origin_view(&mut f);
        let serial = SERIAL_COUNTER.next_serial();
        f.state().raise_and_focus(&target, serial);
        f.state().execute_action(&Action::CloseWindow);
        client_close(&mut f, id, &surface);
        assert!(
            suspended_id(&mut f).is_none(),
            "close-window is a real close"
        );
        assert_eq!(f.state().stage.windows().count(), 0);
    }
    // msg close.
    {
        let tmp = TempDir::new();
        let mut f = Fixture::with_config(config_with("suspend_on_close = true"));
        f.add_output(1, (1920, 1080));
        inject_cache(&mut f, &tmp, &["myapp"]);
        let id = f.add_client();
        let (surface, target) = map_at(&mut f, id, "myapp", (400, 300), (200, 200));
        origin_view(&mut f);
        let win_id = f.state().stage.id_of(&target).unwrap().0;
        let reply =
            crate::ipc::dispatch(Request::Close(Some(WindowSelector::Id(win_id))), f.state());
        assert!(matches!(reply, Ok(Response::Ok)));
        client_close(&mut f, id, &surface);
        assert!(suspended_id(&mut f).is_none(), "msg close is a real close");
        assert_eq!(f.state().stage.windows().count(), 0);
    }
    // Taskbar (foreign-toplevel) close.
    {
        use driftwm::protocols::foreign_toplevel::ForeignToplevelHandler;
        let tmp = TempDir::new();
        let mut f = Fixture::with_config(config_with("suspend_on_close = true"));
        f.add_output(1, (1920, 1080));
        inject_cache(&mut f, &tmp, &["myapp"]);
        let id = f.add_client();
        let (surface, target) = map_at(&mut f, id, "myapp", (400, 300), (200, 200));
        origin_view(&mut f);
        let server = server_surface(&target);
        f.state().close(server);
        client_close(&mut f, id, &surface);
        assert!(
            suspended_id(&mut f).is_none(),
            "a taskbar close is a real close"
        );
        assert_eq!(f.state().stage.windows().count(), 0);
    }
}

/// A dialog — a toplevel with a parent (dead or alive) — is ineligible, so its
/// close never leaves a dialog-shaped stand-in.
#[test]
fn suspend_on_close_dialog_with_parent_does_not_convert() {
    let tmp = TempDir::new();
    let mut f = Fixture::with_config(config_with("suspend_on_close = true"));
    f.add_output(1, (1920, 1080));
    inject_cache(&mut f, &tmp, &["parent", "dialog"]);
    // Parent + dialog share one client — a toplevel's parent must be its own
    // client's toplevel.
    let cid = f.add_client();
    let (parent_surface, parent) = map_at(&mut f, cid, "parent", (400, 300), (100, 100));
    let parent_toplevel = f.client(cid).window(&parent_surface).xdg_toplevel.clone();

    // A child toplevel that names `parent` before its first commit.
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
    origin_view(&mut f);

    client_close(&mut f, cid, &dsurface);
    assert!(
        f.state().stage.windows().all(|w| w.suspended().is_none()),
        "a dialog with a parent must not convert"
    );

    // The parent itself is eligible — close it for real so nothing leaks.
    f.state().mark_real_close(&parent);
    client_close(&mut f, cid, &parent_surface);
}

/// A dialog is ineligible for the explicit `suspend-window` action too:
/// focusing one and firing the action is a no-op, and `msg suspend` on it
/// returns an error (aligning the code with the docs).
#[test]
fn explicit_suspend_and_msg_suspend_refuse_dialog() {
    let tmp = TempDir::new();
    let mut f = Fixture::with_config(config_with(""));
    f.add_output(1, (1920, 1080));
    inject_cache(&mut f, &tmp, &["parent", "dialog"]);
    let cid = f.add_client();
    let (parent_surface, parent) = map_at(&mut f, cid, "parent", (400, 300), (100, 100));
    let parent_toplevel = f.client(cid).window(&parent_surface).xdg_toplevel.clone();

    // A child toplevel that names `parent`.
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
    origin_view(&mut f);

    let dialog_win = window_by_app_id(&mut f, "dialog").unwrap();
    let serial = SERIAL_COUNTER.next_serial();
    f.state().raise_and_focus(&dialog_win, serial);

    // The explicit action refuses the focused dialog — no mark, no close.
    f.state().execute_action(&Action::SuspendWindow);
    assert_eq!(
        f.state().debug_counters()["suspend_marks"],
        0,
        "no mark recorded for a focused dialog"
    );

    // `msg suspend` on the dialog returns an error, not Ok.
    let dialog_id = f.state().stage.id_of(&dialog_win).unwrap().0;
    let reply = crate::ipc::dispatch(
        Request::Suspend(Some(WindowSelector::Id(dialog_id))),
        f.state(),
    );
    assert!(
        matches!(reply, Err(ref e) if e.contains("dialog")),
        "msg suspend on a dialog errors, got {reply:?}"
    );

    // Flag is off, so plain closes leave no stand-in.
    client_close(&mut f, cid, &dsurface);
    let _ = parent;
    client_close(&mut f, cid, &parent_surface);
    assert_eq!(f.state().stage.windows().count(), 0);
}

/// A per-window rule overrides the global flag both ways: off-rule beats
/// on-global (real close), on-rule beats off-global (convert).
#[test]
fn suspend_on_close_rule_override_wins() {
    // Rule off beats global on.
    {
        let tmp = TempDir::new();
        let mut f = Fixture::with_config(config_with(
            "suspend_on_close = true\n[[window_rules]]\napp_id = \"term\"\nsuspend_on_close = false",
        ));
        f.add_output(1, (1920, 1080));
        inject_cache(&mut f, &tmp, &["term"]);
        let id = f.add_client();
        let (surface, _t) = map_at(&mut f, id, "term", (400, 300), (200, 200));
        origin_view(&mut f);
        client_close(&mut f, id, &surface);
        assert!(suspended_id(&mut f).is_none(), "rule off beats global on");
        assert_eq!(f.state().stage.windows().count(), 0);
    }
    // Rule on beats global off.
    {
        let tmp = TempDir::new();
        let mut f = Fixture::with_config(config_with(
            "suspend_on_close = false\n[[window_rules]]\napp_id = \"keepme\"\nsuspend_on_close = true",
        ));
        f.add_output(1, (1920, 1080));
        inject_cache(&mut f, &tmp, &["keepme"]);
        let id = f.add_client();
        let (surface, _t) = map_at(&mut f, id, "keepme", (400, 300), (200, 200));
        origin_view(&mut f);
        client_close(&mut f, id, &surface);
        let sid = suspended_id(&mut f).expect("rule on beats global off");
        f.state().dismiss_suspended(sid);
    }
}

/// Terminal entries and widgets are ineligible for `suspend_on_close`.
#[test]
fn suspend_on_close_terminal_and_widget_ineligible() {
    // A Terminal=true entry fails resolution.
    {
        let tmp = TempDir::new();
        std::fs::write(
            tmp.path().join("shellapp.desktop"),
            "[Desktop Entry]\nType=Application\nName=Shell\nExec=shellapp\nTerminal=true\n",
        )
        .unwrap();
        let mut f = Fixture::with_config(config_with("suspend_on_close = true"));
        f.add_output(1, (1920, 1080));
        f.state().desktop_entry_cache =
            Some(DesktopEntryCache::new(vec![tmp.path().to_path_buf()]));
        let id = f.add_client();
        let (surface, _t) = map_at(&mut f, id, "shellapp", (400, 300), (200, 200));
        origin_view(&mut f);
        client_close(&mut f, id, &surface);
        assert!(
            suspended_id(&mut f).is_none(),
            "Terminal=true is ineligible"
        );
        assert_eq!(f.state().stage.windows().count(), 0);
    }
    // A widget is ineligible.
    {
        let tmp = TempDir::new();
        let mut f = Fixture::with_config(config_with(
            "suspend_on_close = true\n[[window_rules]]\napp_id = \"panel\"\nwidget = true",
        ));
        f.add_output(1, (1920, 1080));
        inject_cache(&mut f, &tmp, &["panel"]);
        let id = f.add_client();
        let (surface, _t) = map_at(&mut f, id, "panel", (400, 300), (200, 200));
        origin_view(&mut f);
        client_close(&mut f, id, &surface);
        assert!(suspended_id(&mut f).is_none(), "a widget is ineligible");
        assert_eq!(f.state().stage.windows().count(), 0);
    }
}

/// The markless geometry prefers a settled `stable_snap_rects` entry when the
/// live geometry shrank (foot's teardown), and trusts live otherwise.
#[test]
fn suspend_on_close_geometry_prefers_stable_when_live_shrinks() {
    // stable larger than live → the stand-in keeps the stable (settled) size.
    {
        let tmp = TempDir::new();
        let mut f = Fixture::with_config(config_with("suspend_on_close = true"));
        f.add_output(1, (1920, 1080));
        inject_cache(&mut f, &tmp, &["myapp"]);
        let id = f.add_client();
        let (surface, target) = map_at(&mut f, id, "myapp", (800, 600), (100, 100));
        origin_view(&mut f);
        seed_stable_rect(&mut f, &target, (100, 100), (900, 700));
        client_close(&mut f, id, &surface);
        let sid = suspended_id(&mut f).unwrap();
        assert_eq!(
            f.state().find_suspended(sid).unwrap().size.get(),
            Size::from((900, 700)),
            "a live shrink (800x600 < 900x700) falls back to the stable rect"
        );
        f.state().dismiss_suspended(sid);
    }
    // stable smaller than live → live is authoritative (cached rect is stale).
    {
        let tmp = TempDir::new();
        let mut f = Fixture::with_config(config_with("suspend_on_close = true"));
        f.add_output(1, (1920, 1080));
        inject_cache(&mut f, &tmp, &["myapp"]);
        let id = f.add_client();
        let (surface, target) = map_at(&mut f, id, "myapp", (800, 600), (100, 100));
        origin_view(&mut f);
        seed_stable_rect(&mut f, &target, (100, 100), (400, 300));
        client_close(&mut f, id, &surface);
        let sid = suspended_id(&mut f).unwrap();
        assert_eq!(
            f.state().find_suspended(sid).unwrap().size.get(),
            Size::from((800, 600)),
            "live is trusted when it is not smaller than the stable rect"
        );
        f.state().dismiss_suspended(sid);
    }
}

/// Seed `stable_snap_rects` with the inflated frame for a body at `pos`/`size`,
/// matching `window_snap_rect`'s inflation (bar on y_low, border all sides).
fn seed_stable_rect(
    f: &mut Fixture,
    window: &smithay::desktop::Window,
    pos: (i32, i32),
    size: (i32, i32),
) {
    let bar = f
        .state()
        .window_ssd_bar(&StageWindow::Client(window.clone()));
    let surface = server_surface(window);
    let bw = f.state().window_border_width(&surface);
    let rect = SnapRect {
        x_low: (pos.0 - bw) as f64,
        x_high: (pos.0 + size.0 + bw) as f64,
        y_low: (pos.1 - bar - bw) as f64,
        y_high: (pos.1 + size.1 + bw) as f64,
    };
    f.state().stable_snap_rects.insert(surface.id(), rect);
}

/// `close-window` and `msg close` on a focused suspended window dismiss it.
#[test]
fn close_binding_and_msg_close_dismiss_suspended() {
    // close-window binding.
    {
        let mut f = Fixture::with_config(config_with(""));
        f.add_output(1, (1920, 1080));
        origin_view(&mut f);
        let sid = f.state().insert_suspended_for_test(
            1,
            Point::from((400, 300)),
            Size::from((300, 200)),
            "s",
            "S",
        );
        f.state().focus_and_raise_suspended(sid);
        assert_eq!(f.state().gated_suspended_focus(), Some(sid));
        f.state().execute_action(&Action::CloseWindow);
        assert!(
            f.state().find_suspended(sid).is_none(),
            "close-window dismissed the suspended window"
        );
    }
    // msg close by id.
    {
        let mut f = Fixture::with_config(config_with(""));
        f.add_output(1, (1920, 1080));
        origin_view(&mut f);
        let sid = f.state().insert_suspended_for_test(
            2,
            Point::from((400, 300)),
            Size::from((300, 200)),
            "s",
            "S",
        );
        let element = StageWindow::Suspended(f.state().find_suspended(sid).unwrap());
        let ipc_id = f.state().stage.id_of(&element).unwrap().0;
        let reply =
            crate::ipc::dispatch(Request::Close(Some(WindowSelector::Id(ipc_id))), f.state());
        assert!(matches!(reply, Ok(Response::Ok)));
        assert!(
            f.state().find_suspended(sid).is_none(),
            "msg close dismissed the suspended window"
        );
    }
}

/// The `suspend-window` action on a focused suspended window dismisses it —
/// including a materialized (restored) stand-in that never had a live client
/// this session.
#[test]
fn suspend_action_on_suspended_focus_dismisses() {
    let mut f = Fixture::with_config(config_with(""));
    f.add_output(1, (1920, 1080));
    origin_view(&mut f);
    let sid = f.state().insert_suspended_for_test(
        1,
        Point::from((400, 300)),
        Size::from((300, 200)),
        "s",
        "S",
    );
    f.state().focus_and_raise_suspended(sid);

    f.state().execute_action(&Action::SuspendWindow);
    assert!(
        f.state().find_suspended(sid).is_none(),
        "the put-away press dismissed the stand-in"
    );
    assert_eq!(f.state().stage.windows().count(), 0);
    assert_eq!(f.state().debug_counters()["suspend_marks"], 0);
}

/// `fill-window` on a focused suspended window is a no-op: the action targets
/// `focused_window()`, which is `None` while a stand-in holds focus intent, so
/// the stand-in is never grown or repositioned.
#[test]
fn fill_action_on_suspended_focus_is_noop() {
    let mut f = Fixture::with_config(config_with(""));
    f.add_output(1, (1920, 1080));
    origin_view(&mut f);
    let sid = f.state().insert_suspended_for_test(
        1,
        Point::from((400, 300)),
        Size::from((300, 200)),
        "s",
        "S",
    );
    f.state().focus_and_raise_suspended(sid);

    let elem = StageWindow::Suspended(f.state().find_suspended(sid).unwrap());
    let before_count = f.state().stage.windows().count();
    let before_size = f.state().find_suspended(sid).unwrap().size.get();
    let before_pos = f.state().stage.position_of(&elem);

    f.state().execute_action(&Action::FillWindow);

    assert_eq!(
        f.state().find_suspended(sid).unwrap().size.get(),
        before_size,
        "stand-in not grown"
    );
    assert_eq!(
        f.state().stage.position_of(&elem),
        before_pos,
        "stand-in not moved"
    );
    assert_eq!(f.state().stage.windows().count(), before_count);

    f.state().dismiss_suspended(sid);
}

/// Fill state is cleared at suspend conversion, so it doesn't ride
/// `Stage::replace` into the stand-in and then the adopted window (which would
/// silently make the relaunched client `is_fill` with a stale pre-fill rect).
#[test]
fn suspend_clears_fill_state_through_adoption() {
    let tmp = TempDir::new();
    let mut f = Fixture::with_config(config_with("suspend_on_close = true"));
    f.add_output(1, (1920, 1080));
    inject_cache(&mut f, &tmp, &["myapp"]);
    let id = f.add_client();
    let (surface, target) = map_at(&mut f, id, "myapp", (400, 300), (200, 200));
    origin_view(&mut f);

    // A filled window: a saved pre-fill rect on its stage entry.
    f.state()
        .stage
        .set_fill(&target, Point::from((10, 10)), Size::from((100, 100)));
    assert!(f.state().stage.is_fill(&target));

    // Client self-close converts under the flag; the conversion clears fill.
    client_close(&mut f, id, &surface);
    let sid = suspended_id(&mut f).expect("converted");
    let stand_in = StageWindow::Suspended(f.state().find_suspended(sid).unwrap());
    assert!(
        !f.state().stage.is_fill(&stand_in),
        "conversion cleared the fill state off the stand-in"
    );

    // Relaunch + identity-fallback adopt with a fresh client.
    assert!(f.state().relaunch_suspended(sid));
    let id2 = f.add_client();
    let surface2 = map_window(&mut f, id2, "myapp", (300, 200));
    let adopted = window_by_app_id(&mut f, "myapp").expect("adopted");
    assert!(
        !f.state().stage.is_fill(&adopted),
        "the adopted window did not inherit stale fill state"
    );

    // Flag is on, so a plain close would re-convert — close it for real.
    f.state().mark_real_close(&adopted);
    client_close(&mut f, id2, &surface2);
    assert_eq!(f.state().stage.windows().count(), 0);
}

/// The IPC inventory reports suspended windows with `suspended: true`, and a
/// focused stand-in is `windows[0]` / `is_focused` per the shared convention.
#[test]
fn inventory_reports_suspended_and_focused_convention() {
    let mut f = Fixture::with_config(config_with(""));
    f.add_output(1, (1920, 1080));
    let cid = f.add_client();
    map_window(&mut f, cid, "live", (300, 200));
    origin_view(&mut f);
    let sid = f.state().insert_suspended_for_test(
        1,
        Point::from((600, 400)),
        Size::from((320, 240)),
        "susapp",
        "Sus App",
    );

    // Not focused: present with the flag, but not first.
    let inv = f.state().window_inventory();
    let sus = inv
        .iter()
        .find(|w| w.suspended)
        .expect("stand-in in inventory");
    assert_eq!(sus.app_id, "susapp");
    assert_eq!(sus.size, [320, 240]);
    assert!(!sus.is_focused);

    // Focused: windows[0] and is_focused.
    f.state().focus_and_raise_suspended(sid);
    let inv = f.state().window_inventory();
    assert!(
        inv[0].suspended && inv[0].is_focused,
        "a focused stand-in leads the inventory"
    );

    f.state().dismiss_suspended(sid);
    close_client(&mut f, cid);
}

/// `msg focus` (no selector) reports a focused stand-in; `msg focus <id>`
/// navigates to one; `msg move` reads and writes its canvas position.
#[test]
fn ipc_focus_and_move_route_to_suspended() {
    let mut f = Fixture::with_config(config_with(""));
    f.add_output(1, (1920, 1080));
    origin_view(&mut f);
    let sid = f.state().insert_suspended_for_test(
        7,
        Point::from((300, 200)),
        Size::from((320, 240)),
        "susapp",
        "Sus App",
    );
    let element = StageWindow::Suspended(f.state().find_suspended(sid).unwrap());
    let ipc_id = f.state().stage.id_of(&element).unwrap().0;

    // msg focus <id> navigates to the stand-in and echoes its handle.
    let reply = crate::ipc::dispatch(Request::Focus(Some(WindowSelector::Id(ipc_id))), f.state());
    match reply {
        Ok(Response::Focused(Some(info))) => {
            assert_eq!(info.id, ipc_id);
            assert_eq!(info.app_id.as_deref(), Some("susapp"));
        }
        other => panic!("expected a Focused reply, got {other:?}"),
    }
    assert_eq!(f.state().gated_suspended_focus(), Some(sid));

    // A no-selector msg focus now reports the focused stand-in.
    let reply = crate::ipc::dispatch(Request::Focus(None), f.state());
    assert!(matches!(
        reply,
        Ok(Response::Focused(Some(info))) if info.id == ipc_id
    ));

    // msg move sets and reads back the canvas position.
    let reply = crate::ipc::dispatch(
        Request::Move {
            window: Some(WindowSelector::Id(ipc_id)),
            to: Some((1000, -500)),
        },
        f.state(),
    );
    assert!(matches!(reply, Ok(Response::Position { x: 1000, y: -500 })));
    let read = crate::ipc::dispatch(
        Request::Move {
            window: Some(WindowSelector::Id(ipc_id)),
            to: None,
        },
        f.state(),
    );
    assert!(matches!(read, Ok(Response::Position { x: 1000, y: -500 })));

    f.state().dismiss_suspended(sid);
}

/// `msg suspend <id>` reaches a window other than the currently-focused one:
/// it focuses the target first, then runs the same path as the
/// `suspend-window` binding.
#[test]
fn ipc_suspend_routes_to_suspend_action() {
    let tmp = TempDir::new();
    let mut f = Fixture::with_config(config_with(""));
    f.add_output(1, (1920, 1080));
    inject_cache(&mut f, &tmp, &["myapp"]);
    let id = f.add_client();
    let (surface, target) = map_at(&mut f, id, "myapp", (400, 300), (200, 200));
    origin_view(&mut f);

    // A different window holds focus; `msg suspend <id>` must still reach
    // `myapp`, not the focused one.
    let id2 = f.add_client();
    map_window(&mut f, id2, "other", (200, 200));
    let other = window_by_app_id(&mut f, "other").unwrap();
    let serial = SERIAL_COUNTER.next_serial();
    f.state().raise_and_focus(&other, serial);

    let win_id = f.state().stage.id_of(&target).unwrap().0;
    let reply = crate::ipc::dispatch(
        Request::Suspend(Some(WindowSelector::Id(win_id))),
        f.state(),
    );
    assert!(matches!(reply, Ok(Response::Ok)));
    assert_eq!(
        f.state().debug_counters()["suspend_marks"],
        1,
        "the selected window was marked, not the previously-focused one"
    );

    client_close(&mut f, id, &surface);
    let sid = suspended_id(&mut f).expect("myapp converted");
    assert_eq!(
        f.state().find_suspended(sid).unwrap().identity.app_id,
        "myapp"
    );

    f.state().dismiss_suspended(sid);
    close_client(&mut f, id2);
}

/// `msg suspend` rejects a widget (nothing to leave a stand-in for) and a
/// selector that already names a suspended stand-in (no client left to close).
#[test]
fn ipc_suspend_rejects_widget_and_already_suspended() {
    // Widget.
    {
        let mut f = Fixture::with_config(config_with(
            "[[window_rules]]\napp_id = \"panel\"\nwidget = true",
        ));
        f.add_output(1, (1920, 1080));
        let id = f.add_client();
        let (surface, target) = map_at(&mut f, id, "panel", (400, 300), (200, 200));
        origin_view(&mut f);
        let win_id = f.state().stage.id_of(&target).unwrap().0;
        let reply = crate::ipc::dispatch(
            Request::Suspend(Some(WindowSelector::Id(win_id))),
            f.state(),
        );
        assert!(reply.is_err(), "a widget cannot be suspended");
        client_close(&mut f, id, &surface);
    }
    // Already suspended.
    {
        let mut f = Fixture::with_config(config_with(""));
        f.add_output(1, (1920, 1080));
        origin_view(&mut f);
        let sid = f.state().insert_suspended_for_test(
            1,
            Point::from((300, 200)),
            Size::from((320, 240)),
            "s",
            "S",
        );
        let element = StageWindow::Suspended(f.state().find_suspended(sid).unwrap());
        let ipc_id = f.state().stage.id_of(&element).unwrap().0;
        let reply = crate::ipc::dispatch(
            Request::Suspend(Some(WindowSelector::Id(ipc_id))),
            f.state(),
        );
        assert!(
            reply.is_err(),
            "an already-suspended stand-in cannot be re-suspended"
        );
        f.state().dismiss_suspended(sid);
    }
}

/// `screenshot all` frames suspended stand-ins: a mix of live + suspended far
/// apart yields a region covering both, and an all-suspended canvas resolves a
/// valid region instead of erroring "no windows to capture".
#[test]
fn screenshot_all_frames_suspended_windows() {
    use crate::ipc::protocol::ScreenshotTarget;

    // Mixed: a live client far left, a stand-in far right.
    {
        let mut f = Fixture::with_config(config_with(""));
        f.add_output(1, (1920, 1080));
        let cid = f.add_client();
        map_window(&mut f, cid, "live", (300, 200));
        let live = window_by_app_id(&mut f, "live").unwrap();
        origin_view(&mut f);
        f.state()
            .map_window(StageWindow::Client(live), Point::from((-2000, 0)), true);
        let sid = f.state().insert_suspended_for_test(
            1,
            Point::from((2000, 0)),
            Size::from((300, 200)),
            "sus",
            "Sus",
        );

        let (region, isolate) =
            crate::ipc::resolve_screenshot_region(&ScreenshotTarget::All, f.state()).unwrap();
        assert!(isolate.is_none(), "an `all` capture composes every window");
        assert!(
            region.loc.x <= -2000,
            "region reaches the live client at the left: {region:?}"
        );
        assert!(
            region.loc.x + region.size.w >= 2300,
            "region reaches the stand-in at the right: {region:?}"
        );

        f.state().dismiss_suspended(sid);
        close_client(&mut f, cid);
    }
    // All-suspended canvas: a valid region, not an error.
    {
        let mut f = Fixture::with_config(config_with(""));
        f.add_output(1, (1920, 1080));
        origin_view(&mut f);
        let sid = f.state().insert_suspended_for_test(
            1,
            Point::from((100, 100)),
            Size::from((300, 200)),
            "sus",
            "Sus",
        );
        let resolved = crate::ipc::resolve_screenshot_region(&ScreenshotTarget::All, f.state());
        assert!(
            resolved.is_ok(),
            "an all-suspended canvas frames the stand-ins, got {resolved:?}"
        );
        f.state().dismiss_suspended(sid);
    }
}

/// `msg screenshot window` resolves a suspended stand-in by id: a non-empty
/// region and a suspended isolate target (docs promise suspended ids screenshot).
#[test]
fn screenshot_window_resolves_suspended_by_id() {
    use crate::ipc::protocol::ScreenshotTarget;

    let mut f = Fixture::with_config(config_with(""));
    f.add_output(1, (1920, 1080));
    origin_view(&mut f);
    let sid = f.state().insert_suspended_for_test(
        7,
        Point::from((300, 200)),
        Size::from((320, 240)),
        "sus",
        "Sus",
    );
    let element = StageWindow::Suspended(f.state().find_suspended(sid).unwrap());
    let ipc_id = f.state().stage.id_of(&element).unwrap().0;

    let (region, isolate) = crate::ipc::resolve_screenshot_region(
        &ScreenshotTarget::Window {
            window: Some(WindowSelector::Id(ipc_id)),
        },
        f.state(),
    )
    .unwrap();
    assert!(
        region.size.w > 0 && region.size.h > 0,
        "non-empty region: {region:?}"
    );
    assert!(
        matches!(isolate, Some(StageWindow::Suspended(ref s)) if s.id == sid),
        "the isolate target is the stand-in"
    );

    f.state().dismiss_suspended(sid);
}

/// The mark maps are exposed as debug counters (leak tracking + fixture
/// baseline).
#[test]
fn debug_counters_include_mark_maps() {
    let mut f = Fixture::with_config(config_with(""));
    let counters = f.state().debug_counters();
    assert!(counters.contains_key("suspend_marks"));
    assert!(counters.contains_key("real_close_marks"));
}
