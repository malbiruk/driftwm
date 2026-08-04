//! Output hotplug (connect/disconnect) policy driven through real clients.
//! Every server dispatch runs the stage invariant check, so a leaked stage
//! entry against a dead output aborts the scenario even without an explicit
//! assert. Client-side `wl_surface` enter/leave aren't recorded, so window
//! membership is asserted server-side via `output_for_window`.

use smithay::utils::Point;

use super::{Fixture, config, map_window, window_by_app_id};

/// A window on the surviving output keeps its stage entry when a different
/// output is unplugged.
#[test]
fn window_survives_output_removal() {
    let mut f = Fixture::new();
    let out1 = f.add_output(1, (1920, 1080));
    let out2 = f.add_output(2, (1280, 720));
    let id = f.add_client();

    map_window(&mut f, id, "app", (400, 300));
    let window = window_by_app_id(&mut f, "app").unwrap();
    assert_eq!(f.state().stage.windows().count(), 1);

    f.remove_output(&out2);

    assert_eq!(f.state().stage.windows().count(), 1);
    assert!(window_by_app_id(&mut f, "app").is_some());
    assert!(f.state().stage.position_of(&window).is_some());
    assert_eq!(
        f.state().output_for_window(&window).map(|o| o.name()),
        Some(out1.name())
    );
}

/// A window fullscreen on the removed output exits fullscreen and lands on the
/// survivor; the client sees a configure without the Fullscreen state.
#[test]
fn fullscreen_on_removed_output_exits_to_survivor() {
    let mut f = Fixture::with_config(config(
        r#"
[[window_rules]]
app_id = "fs"
output = "HEADLESS-2"
"#,
    ));
    let out1 = f.add_output(1, (1920, 1080));
    let out2 = f.add_output(2, (1280, 720));
    let id = f.add_client();

    let surface = map_window(&mut f, id, "fs", (400, 300));
    let window = window_by_app_id(&mut f, "fs").unwrap();

    // Client requests fullscreen with no output; the rule directs it to HEADLESS-2.
    f.client(id).window(&surface).set_fullscreen(None);
    f.double_roundtrip(id);
    assert_eq!(
        f.state().stage.fullscreen_output_of(&window),
        Some("HEADLESS-2")
    );

    // Discard configures seen so far so the post-removal ones stand alone.
    let _ = f.client(id).window(&surface).format_recent_configures();

    f.remove_output(&out2);
    f.double_roundtrip(id);

    assert!(!f.state().stage.has_fullscreen());
    assert!(f.state().stage.fullscreen_output_of(&window).is_none());

    let post = f.client(id).window(&surface).format_recent_configures();
    assert!(
        !post.is_empty(),
        "expected an exit configure after the output was removed"
    );
    assert!(
        !post.contains("Fullscreen"),
        "exit configure must not carry Fullscreen, got:\n{post}"
    );

    assert_eq!(
        f.state().output_for_window(&window).map(|o| o.name()),
        Some(out1.name())
    );
}

/// A screen-pinned window on the removed output reassigns to the survivor with
/// its screen position clamped into the smaller survivor's bounds.
#[test]
fn pinned_on_removed_output_reassigns() {
    let mut f = Fixture::with_config(config(
        r#"
[[window_rules]]
app_id = "pin"
pinned_to_screen = true
size = [320, 240]
"#,
    ));
    // Add HEADLESS-2 first so it's the focused/active output the pin binds to;
    // HEADLESS-1 is the smaller survivor, forcing a clamp on reassignment.
    let out2 = f.add_output(2, (1920, 1080));
    let _out1 = f.add_output(1, (800, 600));
    let id = f.add_client();

    map_window(&mut f, id, "pin", (320, 240));
    let window = window_by_app_id(&mut f, "pin").unwrap();
    let site = f.state().stage.pin_of(&window).cloned().unwrap();
    assert_eq!(site.output, "HEADLESS-2");
    // Output center: (1920/2 - 320/2, 1080/2 - 240/2).
    assert_eq!(site.screen_pos, Point::from((800, 420)));

    f.remove_output(&out2);

    let site = f.state().stage.pin_of(&window).cloned().unwrap();
    assert_eq!(site.output, "HEADLESS-1");
    // Clamped into the 800×600 survivor for the 320×240 window:
    // x -> min(800, 800-320)=480, y -> min(420, 600-240)=360.
    assert_eq!(site.screen_pos, Point::from((480, 360)));
}

/// Removing the last output keeps a virtual placeholder (so `active_output()`
/// stays Some and the window survives); reconnecting a new output retires the
/// placeholder and re-homes the window.
#[test]
fn last_output_removal_leaves_placeholder() {
    let mut f = Fixture::new();
    let out1 = f.add_output(1, (1920, 1080));
    let id = f.add_client();

    map_window(&mut f, id, "app", (400, 300));
    let window = window_by_app_id(&mut f, "app").unwrap();
    let pos_before = f.state().stage.position_of(&window).unwrap();

    f.remove_output(&out1);

    assert!(f.state().disconnected_outputs.contains("HEADLESS-1"));
    assert!(f.state().active_output().is_some());
    assert_eq!(f.state().stage.windows().count(), 1);
    assert_eq!(f.state().stage.position_of(&window), Some(pos_before));

    // Reconnect with a fresh name: the placeholder retires and the window
    // re-homes onto the new output.
    let out2 = f.add_output(2, (1280, 720));
    f.roundtrip(id);

    assert!(f.state().disconnected_outputs.is_empty());
    assert_eq!(f.state().stage.windows().count(), 1);
    assert_eq!(
        f.state().output_for_window(&window).map(|o| o.name()),
        Some(out2.name())
    );
}

/// Fullscreen entered while only a placeholder is attached must not leak a stage
/// entry against the dead output name: reconnecting exits it cleanly.
#[test]
fn fullscreen_survives_unplug_replug_without_leak() {
    let mut f = Fixture::new();
    let out1 = f.add_output(1, (1920, 1080));
    let id = f.add_client();

    let surface = map_window(&mut f, id, "app", (800, 600));
    let window = window_by_app_id(&mut f, "app").unwrap();

    f.client(id).window(&surface).set_fullscreen(None);
    f.double_roundtrip(id);
    f.client(id).window(&surface).ack_last_and_commit();
    f.double_roundtrip(id);
    assert!(f.state().stage.has_fullscreen());

    // Unplug the only output: is_last exits fullscreen and keeps a placeholder.
    f.remove_output(&out1);
    assert!(!f.state().stage.has_fullscreen());
    assert!(f.state().disconnected_outputs.contains("HEADLESS-1"));

    // The window re-enters fullscreen while only the placeholder is attached.
    f.client(id).window(&surface).set_fullscreen(None);
    f.double_roundtrip(id);
    assert!(f.state().stage.has_fullscreen());
    assert_eq!(
        f.state().stage.fullscreen_output_of(&window),
        Some("HEADLESS-1")
    );

    // Reconnect: retiring the placeholder must exit the fullscreen held against
    // the dead name — no stage entry, no stranded fullscreen_return.
    let out2 = f.add_output(2, (1280, 720));
    assert!(f.state().disconnected_outputs.is_empty());
    assert!(!f.state().stage.has_fullscreen());
    assert!(f.state().stage.fullscreen_output_of(&window).is_none());
    for output in f.state().space.outputs().cloned().collect::<Vec<_>>() {
        assert!(
            crate::state::output_state(&output)
                .fullscreen_return
                .is_none()
        );
    }
    assert_eq!(
        f.state().output_for_window(&window).map(|o| o.name()),
        Some(out2.name())
    );
}

/// Removing the focused output transfers focus to a survivor.
#[test]
fn focus_and_pointer_move_to_survivor() {
    let mut f = Fixture::new();
    let out1 = f.add_output(1, (1920, 1080));
    let out2 = f.add_output(2, (1280, 720));

    // Simulate the pointer sitting on the second output; move it off the
    // survivor's center so the disconnect warp is observable.
    f.state().focused_output = Some(out2.clone());
    f.state().warp_pointer((2400.0, 300.0).into());
    assert_eq!(
        f.state().active_output().map(|o| o.name()),
        Some(out2.name())
    );

    f.remove_output(&out2);

    assert_eq!(
        f.state().focused_output.as_ref().map(|o| o.name()),
        Some(out1.name())
    );
    // Warped to the survivor's viewport center: camera (-960, -540) at zoom 1
    // over a 1920×1080 output.
    let pointer = f.state().seat.get_pointer().unwrap().current_location();
    assert_eq!((pointer.x, pointer.y), (0.0, 0.0));
}

/// A pin suspended by fullscreen rebinds by its *visual frame* too. The saved
/// site is invisible to `pinned_windows()`, so it takes a pass of its own —
/// clamped as content, the fullscreen exit would restore the pin with its title
/// bar off the top of the survivor.
#[test]
fn a_fullscreen_suspended_pin_rebinds_with_its_whole_frame_on_screen() {
    let mut f = Fixture::with_config(config(
        r#"
[decorations]
border_width = 4

[[window_rules]]
app_id = "pin"
pinned_to_screen = true
decoration = "server"
"#,
    ));
    // out1 is active, so the pin binds there; the window then fullscreens on
    // out2, which parks the pin in out2's `fullscreen_return` where the ordinary
    // orphan pass can't see it.
    let out1 = f.add_output(1, (1920, 1080));
    let _out2 = f.add_output(2, (800, 600));
    let id = f.add_client();

    let surface = map_window(&mut f, id, "pin", (320, 240));
    let window = window_by_app_id(&mut f, "pin").unwrap();
    let chrome = f.state().element_chrome(&window);
    assert_eq!(
        (chrome.bar, chrome.border),
        (25, 4),
        "precondition: the pin wears a bar and a border"
    );
    assert_eq!(
        f.state().stage.pin_of(&window).unwrap().output,
        "HEADLESS-1"
    );

    let target = f.client(id).output("HEADLESS-2");
    f.client(id).window(&surface).set_fullscreen(Some(&target));
    f.double_roundtrip(id);
    assert_eq!(
        f.state().stage.fullscreen_output_of(&window),
        Some("HEADLESS-2")
    );
    assert!(
        f.state().stage.pin_of(&window).is_none(),
        "precondition: the fullscreen suspended the pin"
    );

    f.remove_output(&out1);

    let survivor = f.state().space.outputs().next().cloned().unwrap();
    let saved = crate::state::output_state(&survivor)
        .fullscreen_return
        .as_ref()
        .and_then(|r| r.pinned.clone())
        .expect("the suspended pin survives the unplug");
    assert_eq!(saved.output, "HEADLESS-2");

    // Clamped against the pre-fullscreen 320x240 size, so the frame is 328x273
    // and fits the 800x600 survivor exactly at its bottom-right corner.
    let frame = chrome.frame_loc(saved.screen_pos);
    let frame_size = chrome.frame_size(smithay::utils::Size::from((320, 240)));
    assert!(
        frame.x >= 0 && frame.y >= 0,
        "the frame's top-left stays on the survivor, got {frame:?}"
    );
    assert_eq!(
        (frame.x + frame_size.w, frame.y + frame_size.h),
        (800, 600),
        "the whole frame stays inside the survivor"
    );
}
