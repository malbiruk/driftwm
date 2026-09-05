//! The `pass_mouse` window rule: a compositor mouse binding the window under
//! the pointer claims is discarded, and the press or scroll takes the ordinary
//! unbound path to the app instead. Every scenario asserts from the two sides a
//! user can see — what the client received, and whether the compositor acted
//! (grab, zoom, fullscreen) — never on the lookup that decided it.
//!
//! The rule is re-resolved against the live config on every event, so a reload
//! applies without remapping the window; the SSD title bar stays with the
//! compositor whatever the rule says.

use driftwm::config::{BTN_LEFT, BTN_RIGHT};
use smithay::desktop::Window;
use smithay::utils::{Logical, Point};

use crate::grabs::{MoveGrab, PanGrab, ResizeGrab};
use crate::state::DriftWm;

use super::client::ClientId;
use super::input_backend::{
    FakeDevice, key_press, pointer_to, press, release, trackpad_scroll, wheel_notch_down,
};
use super::{Fixture, config, give_ssd, map_window, motion, window_by_app_id};

/// evdev codes, the space `key_press` reports in. Super is the default
/// `mod_key`, so it is what a `mod+…` binding needs held.
const KEY_LEFTALT: u32 = 56;
const KEY_LEFTMETA: u32 = 125;

/// `wl_pointer.button_state` on the wire, as the client records it.
const PRESSED: u32 = 1;
const RELEASED: u32 = 0;

const CLAIM_ALT_LEFT: &str = r#"
[[window_rules]]
app_id = "app"
pass_mouse = ["alt+left"]
"#;

const CLAIM_ALT_RIGHT: &str = r#"
[[window_rules]]
app_id = "app"
pass_mouse = ["alt+right"]
"#;

const CLAIM_EVERYTHING: &str = r#"
[[window_rules]]
app_id = "app"
pass_mouse = true
"#;

/// The continuous scroll trigger a wheel reports, which is what the default
/// `mod+wheel-scroll` zoom is bound to.
const CLAIM_WHEEL_SCROLL: &str = r#"
[[window_rules]]
app_id = "app"
pass_mouse = ["mod+wheel-scroll"]
"#;

/// A trackpad scrolls under its own trigger, not the wheel's.
const CLAIM_TRACKPAD_SCROLL: &str = r#"
[[window_rules]]
app_id = "app"
pass_mouse = ["mod+trackpad-scroll"]
"#;

/// Pick mode armed with a claim: below the threshold the window has no pointer
/// focus to forward to, so the binding must survive.
const CLAIM_BELOW_INTERACT_MIN: &str = r#"
[zoom]
interact_min = 0.5

[[window_rules]]
app_id = "app"
pass_mouse = ["alt+left"]
"#;

/// The notch site's own pick-mode case: a claim armed below `interact_min`.
const NOTCH_CLAIMED_BELOW_INTERACT_MIN: &str = r#"
[zoom]
interact_min = 0.5

[mouse.on-window]
"alt+wheel-down" = "toggle-fullscreen"

[[window_rules]]
app_id = "app"
pass_mouse = ["alt+wheel-down"]
"#;

const PIN: &str = r#"
[[window_rules]]
app_id = "app"
pinned_to_screen = true
size = [400, 300]
"#;

const PIN_AND_CLAIM: &str = r#"
[[window_rules]]
app_id = "app"
pinned_to_screen = true
size = [400, 300]
pass_mouse = ["alt+left"]
"#;

const PIN_AND_CLAIM_SCROLL: &str = r#"
[[window_rules]]
app_id = "app"
pinned_to_screen = true
size = [400, 300]
pass_mouse = ["mod+wheel-scroll"]
"#;

/// A wheel notch bound to an action, which is the discrete site — a separate
/// lookup from the continuous `wheel-scroll` one below it.
const NOTCH_FULLSCREEN: &str = r#"
[mouse.on-window]
"alt+wheel-down" = "toggle-fullscreen"
"#;

const NOTCH_FULLSCREEN_CLAIMED: &str = r#"
[mouse.on-window]
"alt+wheel-down" = "toggle-fullscreen"

[[window_rules]]
app_id = "app"
pass_mouse = ["alt+wheel-down"]
"#;

/// Canvas-space center of `window`'s current geometry.
fn center_of(f: &mut Fixture, window: &Window) -> Point<f64, Logical> {
    let pos = f.state().stage.position_of(window).expect("staged");
    let size = window.geometry().size;
    Point::from((
        pos.x as f64 + size.w as f64 / 2.0,
        pos.y as f64 + size.h as f64 / 2.0,
    ))
}

fn grab_is<G: smithay::input::pointer::PointerGrab<DriftWm>>(f: &mut Fixture) -> bool {
    f.state()
        .seat
        .get_pointer()
        .unwrap()
        .with_grab(|_, g| g.is::<G>())
        .unwrap_or(false)
}

/// Every `wl_pointer.button` the client has received so far.
fn client_buttons(f: &mut Fixture, id: ClientId) -> Vec<(u32, u32)> {
    f.double_roundtrip(id);
    f.client(id).state.pointer_buttons.clone()
}

/// The vertical scroll amounts the client has received so far.
fn client_axes(f: &mut Fixture, id: ClientId) -> Vec<f64> {
    f.double_roundtrip(id);
    f.client(id).state.pointer_axes.clone()
}

/// A single mapped 400x300 window on one output.
fn mapped_window(f: &mut Fixture) -> (ClientId, Window) {
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    map_window(f, id, "app", (400, 300));
    let window = window_by_app_id(f, "app").unwrap();
    // Mapping aims the navigate-to animation at the new window, so the zoom
    // scenarios below would read its target rather than the scroll's.
    f.state().with_output_state(|os| {
        os.camera_target = None;
        os.zoom_target = None;
    });
    (id, window)
}

/// Hold `held` (a modifier keycode) and rest the pointer on canvas-space `at`.
fn aim_and_hold(f: &mut Fixture, held: u32, at: Point<f64, Logical>) {
    key_press(f, held);
    pointer_to(f, &FakeDevice::mouse(), at);
}

/// The middle of `window`'s SSD title bar, which sits above its geometry.
fn title_bar_point(f: &mut Fixture, window: &Window) -> Point<f64, Logical> {
    let loc = f.state().stage.position_of(window).expect("staged");
    let bar = f.state().config.decorations.title_bar_height as f64;
    Point::from((
        loc.x as f64 + window.geometry().size.w as f64 / 2.0,
        loc.y as f64 - bar / 2.0,
    ))
}

/// [`mapped_window`] with the pointer resting on its center and `held` down.
fn window_under_pointer(f: &mut Fixture, held: u32) -> (ClientId, Window) {
    let (id, window) = mapped_window(f);
    let target = center_of(f, &window);
    aim_and_hold(f, held, target);
    (id, window)
}

/// A listed combo over the window is handed to the app: it lands on the client
/// as a real `wl_pointer.button` instead of starting the bound move.
#[test]
fn a_claimed_press_reaches_the_client() {
    let mut f = Fixture::with_config(config(CLAIM_ALT_LEFT));
    let (id, _) = window_under_pointer(&mut f, KEY_LEFTALT);

    press(&mut f, &FakeDevice::mouse(), BTN_LEFT);

    assert_eq!(
        client_buttons(&mut f, id),
        vec![(BTN_LEFT, PRESSED)],
        "the app owns alt+left, so it must see the press"
    );
}

/// The other half of the same press: the compositor's own `alt+left` move
/// binding must not have run. Forwarding a press installs smithay's own click
/// grab, so the observable is the window standing still under a drag, not the
/// absence of a grab.
#[test]
fn a_claimed_drag_does_not_move_the_window() {
    let mut f = Fixture::with_config(config(CLAIM_ALT_LEFT));
    let (_, window) = window_under_pointer(&mut f, KEY_LEFTALT);
    let before = f.state().stage.position_of(&window).expect("staged");

    press(&mut f, &FakeDevice::mouse(), BTN_LEFT);
    let dragged = center_of(&mut f, &window) + Point::from((100.0, 0.0));
    motion(&mut f, dragged);

    assert_eq!(
        f.state().stage.position_of(&window).expect("staged"),
        before,
        "the drag belongs to the app, so the window must not move"
    );
}

/// Control for the two above: with no rule the same drag is the compositor's,
/// so it moves the window and the app never hears about it.
#[test]
fn an_unclaimed_drag_moves_the_window_and_stays_off_the_wire() {
    let mut f = Fixture::new();
    let (id, window) = window_under_pointer(&mut f, KEY_LEFTALT);
    let before = f.state().stage.position_of(&window).expect("staged");

    press(&mut f, &FakeDevice::mouse(), BTN_LEFT);
    let dragged = center_of(&mut f, &window) + Point::from((100.0, 0.0));
    motion(&mut f, dragged);

    assert_ne!(
        f.state().stage.position_of(&window).expect("staged"),
        before,
        "alt+left is the move binding when nothing claims it"
    );
    assert_eq!(
        client_buttons(&mut f, id),
        Vec::new(),
        "a binding the compositor ran must not also reach the app"
    );
}

/// A list claims only what it lists: `alt+right` is still the compositor's
/// resize binding over a window that passed `alt+left`.
#[test]
fn a_combo_outside_the_list_still_binds() {
    let mut f = Fixture::with_config(config(CLAIM_ALT_LEFT));
    window_under_pointer(&mut f, KEY_LEFTALT);

    press(&mut f, &FakeDevice::mouse(), BTN_RIGHT);

    assert!(
        grab_is::<ResizeGrab>(&mut f),
        "alt+right was never listed, so it still resizes"
    );
}

/// `pass_mouse = true` reaches bindings from the `anywhere` context too — the
/// default `mod+left` pan is claimed over the window like any other.
#[test]
fn passing_everything_claims_an_anywhere_binding() {
    let mut f = Fixture::with_config(config(CLAIM_EVERYTHING));
    let (id, _) = window_under_pointer(&mut f, KEY_LEFTMETA);

    press(&mut f, &FakeDevice::mouse(), BTN_LEFT);

    assert_eq!(
        client_buttons(&mut f, id),
        vec![(BTN_LEFT, PRESSED)],
        "mod+left pans anywhere, but this window claimed it"
    );
}

#[test]
fn an_unclaimed_anywhere_binding_still_pans() {
    let mut f = Fixture::new();
    window_under_pointer(&mut f, KEY_LEFTMETA);

    press(&mut f, &FakeDevice::mouse(), BTN_LEFT);

    assert!(grab_is::<PanGrab>(&mut f), "mod+left pans the viewport");
}

/// A claimed scroll forwards as a scroll: the default `mod+wheel-scroll` zoom
/// never fires and the wheel reaches the app.
#[test]
fn a_claimed_scroll_reaches_the_client() {
    let mut f = Fixture::with_config(config(CLAIM_WHEEL_SCROLL));
    let (id, _) = window_under_pointer(&mut f, KEY_LEFTMETA);

    wheel_notch_down(&mut f, &FakeDevice::mouse());

    assert_eq!(
        client_axes(&mut f, id),
        vec![15.0],
        "the app owns the wheel over its own window"
    );
}

#[test]
fn a_claimed_scroll_does_not_zoom() {
    let mut f = Fixture::with_config(config(CLAIM_WHEEL_SCROLL));
    window_under_pointer(&mut f, KEY_LEFTMETA);

    wheel_notch_down(&mut f, &FakeDevice::mouse());

    assert_eq!(
        f.state().zoom_target(),
        None,
        "a claimed scroll must leave the viewport alone"
    );
}

/// Control: without the rule the same wheel notch is the zoom binding.
#[test]
fn an_unclaimed_scroll_zooms() {
    let mut f = Fixture::new();
    let (id, _) = window_under_pointer(&mut f, KEY_LEFTMETA);

    wheel_notch_down(&mut f, &FakeDevice::mouse());

    assert!(
        f.state().zoom_target().is_some(),
        "mod+wheel-scroll zooms the viewport"
    );
    assert!(
        client_axes(&mut f, id).is_empty(),
        "a scroll the zoom consumed must not also reach the app"
    );
}

/// A trackpad scrolls under `trackpad-scroll`, not `wheel-scroll`, and a claim
/// on that trigger hands it over: the default `mod` pan never runs.
#[test]
fn a_claimed_trackpad_scroll_reaches_the_client_without_panning() {
    let mut f = Fixture::with_config(config(CLAIM_TRACKPAD_SCROLL));
    let (id, _) = window_under_pointer(&mut f, KEY_LEFTMETA);
    let before = f.state().camera();

    trackpad_scroll(&mut f, &FakeDevice::touchpad());

    assert_eq!(
        client_axes(&mut f, id),
        vec![15.0],
        "the app owns the trackpad over its own window"
    );
    assert_eq!(
        f.state().camera(),
        before,
        "a claimed trackpad scroll must leave the camera alone"
    );
}

#[test]
fn an_unclaimed_trackpad_scroll_pans() {
    let mut f = Fixture::new();
    window_under_pointer(&mut f, KEY_LEFTMETA);
    let before = f.state().camera();

    trackpad_scroll(&mut f, &FakeDevice::touchpad());

    assert_ne!(
        f.state().camera(),
        before,
        "mod+trackpad-scroll pans the viewport"
    );
}

/// The discrete wheel-notch site is a separate lookup from the continuous one,
/// and a claim covers it too — the bound action never runs.
#[test]
fn a_claimed_wheel_notch_runs_no_action() {
    let mut f = Fixture::with_config(config(NOTCH_FULLSCREEN_CLAIMED));
    let (id, _) = window_under_pointer(&mut f, KEY_LEFTALT);

    wheel_notch_down(&mut f, &FakeDevice::mouse());

    assert!(
        !f.state().is_fullscreen(),
        "the window claimed the notch, so its fullscreen action must not fire"
    );
    assert_eq!(
        client_axes(&mut f, id),
        vec![15.0],
        "the notch forwards to the app instead"
    );
}

#[test]
fn an_unclaimed_wheel_notch_runs_its_action() {
    let mut f = Fixture::with_config(config(NOTCH_FULLSCREEN));
    f.skip_baseline_check();
    window_under_pointer(&mut f, KEY_LEFTALT);

    wheel_notch_down(&mut f, &FakeDevice::mouse());

    assert!(
        f.state().is_fullscreen(),
        "a bound wheel notch runs its action"
    );
}

/// The rule resolves against the live config on every press, so a reload takes
/// effect on a window that was mapped before the rule existed.
#[test]
fn a_reloaded_rule_applies_without_remapping_the_window() {
    let mut f = Fixture::new();
    let (id, _) = window_under_pointer(&mut f, KEY_LEFTALT);

    f.state().reload_config_from_contents(CLAIM_ALT_LEFT);
    press(&mut f, &FakeDevice::mouse(), BTN_LEFT);

    assert_eq!(
        client_buttons(&mut f, id),
        vec![(BTN_LEFT, PRESSED)],
        "the reloaded rule must apply to a window that was already mapped"
    );

    // Every reload queues a mode intent for the output, and only the udev
    // render loop drains it — drain it by hand to end at the leak baseline.
    f.state().pending_mode_changes.clear();
}

/// Compositor chrome is never passed: a claimed combo on the SSD title bar
/// falls through to the ordinary title-bar move, so a `pass_mouse` window can
/// still be dragged by its bar.
#[test]
fn a_claimed_combo_on_the_title_bar_still_moves_the_window() {
    let mut f = Fixture::with_config(config(CLAIM_ALT_LEFT));
    let (id, window) = mapped_window(&mut f);
    give_ssd(&mut f, &window);
    let on_bar = title_bar_point(&mut f, &window);
    aim_and_hold(&mut f, KEY_LEFTALT, on_bar);

    press(&mut f, &FakeDevice::mouse(), BTN_LEFT);

    assert!(
        grab_is::<MoveGrab>(&mut f),
        "the title bar stays compositor-owned whatever pass_mouse says"
    );
    assert_eq!(
        client_buttons(&mut f, id),
        Vec::new(),
        "a chrome click is not the app's"
    );
}

/// The same for a button the decoration branch itself ignores: `alt+right` on
/// the title bar of a claiming window still runs the compositor's resize, which
/// is the half a left-only chrome check would drop.
#[test]
fn a_claimed_combo_on_the_title_bar_still_resizes_the_window() {
    let mut f = Fixture::with_config(config(CLAIM_ALT_RIGHT));
    let (id, window) = mapped_window(&mut f);
    give_ssd(&mut f, &window);
    let on_bar = title_bar_point(&mut f, &window);
    aim_and_hold(&mut f, KEY_LEFTALT, on_bar);

    press(&mut f, &FakeDevice::mouse(), BTN_RIGHT);

    assert!(
        grab_is::<ResizeGrab>(&mut f),
        "the title bar stays compositor-owned for every button, not just left"
    );
    assert_eq!(
        client_buttons(&mut f, id),
        Vec::new(),
        "a chrome click is not the app's"
    );
}

/// Below `interact_min` a canvas window has no pointer focus, so a claim there
/// would leave the click doing nothing at all. The binding wins instead, which
/// is what the same press does with no rule (see `interact_min.rs`).
#[test]
fn a_claim_below_interact_min_leaves_the_binding_alone() {
    let mut f = Fixture::with_config(config(CLAIM_BELOW_INTERACT_MIN));
    let (id, window) = mapped_window(&mut f);
    f.state().set_zoom(0.3);
    let target = center_of(&mut f, &window);
    aim_and_hold(&mut f, KEY_LEFTALT, target);

    press(&mut f, &FakeDevice::mouse(), BTN_LEFT);

    assert!(
        grab_is::<MoveGrab>(&mut f),
        "pick mode keeps alt+left as the move binding"
    );
    assert_eq!(
        client_buttons(&mut f, id),
        Vec::new(),
        "the press the compositor ran must not also reach the app"
    );
}

/// The wheel notch has no OnCanvas retry of its own, so a claim below
/// `interact_min` would leave the notch dead. The bound action runs instead,
/// exactly as it does with no rule at all.
#[test]
fn a_claimed_notch_below_interact_min_still_runs_its_action() {
    let mut f = Fixture::with_config(config(NOTCH_CLAIMED_BELOW_INTERACT_MIN));
    f.skip_baseline_check();
    let (_, window) = mapped_window(&mut f);
    f.state().set_zoom(0.3);
    let target = center_of(&mut f, &window);
    aim_and_hold(&mut f, KEY_LEFTALT, target);

    wheel_notch_down(&mut f, &FakeDevice::mouse());

    assert!(
        f.state().is_fullscreen(),
        "pick mode keeps alt+wheel-down as the fullscreen binding"
    );
}

/// A screen-pinned window is dispatched through its own screen-space arm, which
/// resolves the claim separately. The forward there also installs a
/// `ScreenSpaceClickGrab`, so "the window stayed put" is the observable, not
/// the absence of a grab.
#[test]
fn a_claimed_press_on_a_pinned_window_reaches_the_client() {
    let mut f = Fixture::with_config(config(PIN_AND_CLAIM));
    let (id, _) = window_under_pointer(&mut f, KEY_LEFTALT);

    press(&mut f, &FakeDevice::mouse(), BTN_LEFT);

    assert_eq!(
        client_buttons(&mut f, id),
        vec![(BTN_LEFT, PRESSED)],
        "a pinned window claims its combos like any other"
    );
}

#[test]
fn a_claimed_press_on_a_pinned_window_does_not_drag_it() {
    let mut f = Fixture::with_config(config(PIN_AND_CLAIM));
    let (_, window) = window_under_pointer(&mut f, KEY_LEFTALT);
    let before = f.state().stage.pin_of(&window).unwrap().screen_pos;

    press(&mut f, &FakeDevice::mouse(), BTN_LEFT);
    let dragged = center_of(&mut f, &window) + Point::from((100.0, 0.0));
    motion(&mut f, dragged);

    assert_eq!(
        f.state().stage.pin_of(&window).unwrap().screen_pos,
        before,
        "the drag belongs to the app, so the pin must not move"
    );
}

#[test]
fn an_unclaimed_press_on_a_pinned_window_drags_it() {
    let mut f = Fixture::with_config(config(PIN));
    let (_, window) = window_under_pointer(&mut f, KEY_LEFTALT);
    let before = f.state().stage.pin_of(&window).unwrap().screen_pos;

    press(&mut f, &FakeDevice::mouse(), BTN_LEFT);
    let dragged = center_of(&mut f, &window) + Point::from((100.0, 0.0));
    motion(&mut f, dragged);

    assert_ne!(
        f.state().stage.pin_of(&window).unwrap().screen_pos,
        before,
        "alt+left is the pinned move binding when nothing claims it"
    );
}

/// The pinned arm of the claim's window lookup, which the button path resolves
/// earlier and so never reaches: a scroll over a pinned window still finds it.
#[test]
fn a_claimed_scroll_over_a_pinned_window_does_not_zoom() {
    let mut f = Fixture::with_config(config(PIN_AND_CLAIM_SCROLL));
    let (id, _) = window_under_pointer(&mut f, KEY_LEFTMETA);

    wheel_notch_down(&mut f, &FakeDevice::mouse());

    assert_eq!(
        f.state().zoom_target(),
        None,
        "a pinned window claims its combos like any other"
    );
    assert_eq!(
        client_axes(&mut f, id),
        vec![15.0],
        "the wheel forwards to the app instead"
    );
}

/// A fullscreen window resolves its own claim (the fullscreen branch returns
/// before the canvas one runs), and the forward there leaves fullscreen intact.
#[test]
fn a_claimed_press_on_a_fullscreen_window_reaches_the_client() {
    let mut f = Fixture::with_config(config(CLAIM_ALT_LEFT));
    f.skip_baseline_check();
    let (id, window) = mapped_window(&mut f);
    let output = f.state().active_output().unwrap();
    f.state().enter_fullscreen(&window, Some(output));
    f.double_roundtrip(id);

    let target = center_of(&mut f, &window);
    aim_and_hold(&mut f, KEY_LEFTALT, target);
    press(&mut f, &FakeDevice::mouse(), BTN_LEFT);
    release(&mut f, &FakeDevice::mouse(), BTN_LEFT);

    assert_eq!(
        client_buttons(&mut f, id),
        vec![(BTN_LEFT, PRESSED), (BTN_LEFT, RELEASED)],
        "a claimed click over a fullscreen window is the app's"
    );
    assert!(
        f.state().is_fullscreen(),
        "a claimed click must not exit fullscreen"
    );
}
