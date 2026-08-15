//! `SessionLockHandler::lock` must not leave input pointed at the app behind
//! the lock screen: pointer focus restored by `unset_grab`, a finger already
//! down, or a camera pan a grab's teardown lands after the per-output clear
//! that was supposed to be the last word on it. Also: `lock` refuses to hand
//! a live session over to a second client, but must still let a replacement
//! take over once the incumbent's client has died.
//!
//! Most scenarios here never commit a lock surface, so `session_lock` stays
//! `Pending` throughout — that's already where every defect under test
//! manifests. A second `lock_session()` from the same client's session lock
//! object is refused outright while the first is alive, so the teardown runs
//! once per test; don't call it twice expecting it to run again.

use std::time::{Duration, Instant};

use driftwm::canvas::{CanvasPos, canvas_to_screen};
use smithay::desktop::Window;
use smithay::output::Output;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::Point;

use smithay::wayland::input_method::InputMethodKeyboardGrab;
use smithay::wayland::session_lock::SessionLockHandler;
use wayland_protocols::ext::session_lock::v1::client::ext_session_lock_surface_v1;

use crate::state::session_lock::PENDING_LOCK_DEADLINE;
use crate::state::{SessionLock, StageWindow};

use super::client::{ClientId, LockEvent, TouchEvent};
use super::input_backend::{
    FakeDevice, pointer_relative_motion, pointer_to, pointer_to_screen, touch_cancel, touch_down,
    touch_motion, touch_up,
};
use super::{
    Fixture, give_ssd, keyboard_focus, map_window, pointer_focus, server_surface, window_by_app_id,
};

fn origin_view(f: &mut Fixture) {
    f.state().set_camera(Point::from((0.0, 0.0)));
    f.state().with_output_state(|os| {
        os.zoom = 1.0;
        os.camera = Point::from((0.0, 0.0));
    });
}

/// Like [`origin_view`], but with an explicit camera/zoom — needed wherever a
/// missing coordinate-space conversion would otherwise be numerically
/// invisible under the identity view.
fn custom_view(f: &mut Fixture, camera: Point<f64, smithay::utils::Logical>, zoom: f64) {
    f.state().set_camera(camera);
    f.state().with_output_state(move |os| {
        os.zoom = zoom;
        os.camera = camera;
    });
}

/// A click or scroll landing between `lock()` and the first locked motion must
/// not reach the app behind the lock screen — `unset_grab`'s restored focus
/// has to be cleared by `lock()` itself, not left for the next motion to
/// overwrite.
#[test]
fn lock_clears_pointer_focus_from_the_window_under_the_cursor() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    window_under_pointer(&mut f, id);

    f.client(id).lock_session();
    f.roundtrip(id);

    assert!(f.state().session_lock.is_locked(), "the lock handler ran");
    assert_eq!(
        pointer_focus(&mut f),
        None,
        "pointer focus must not survive lock() pointing at the app behind the lock screen"
    );
}

/// A finger already down when the session locks must not keep reaching the
/// window it landed on — see [`TouchState::lock_slots`] for why neither
/// smithay's own touch-focus routing nor `cancel()` stops it on their own,
/// and why the app never sees this touch point end.
///
/// [`TouchState::lock_slots`]: crate::input::touch::TouchState::lock_slots
#[test]
fn lock_stops_a_settled_touch_from_reaching_the_window_it_landed_on() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    map_window(&mut f, id, "a", (400, 300));
    let window = window_by_app_id(&mut f, "a").unwrap();
    origin_view(&mut f);

    let center = center_of(&mut f, &window);
    touch_down(&mut f, center, 0);
    f.double_roundtrip(id);

    assert!(
        f.state().seat.get_touch().unwrap().is_grabbed(),
        "precondition: the finger down installed a live touch grab"
    );

    // The down is withheld from the wire for `HOLDBACK_MS`, in case a second
    // finger turns this into a multi-finger gesture — wait past the deadline
    // so the finger is a genuine, server-tracked touch before locking, not
    // merely a withheld event our own buffer would otherwise discard.
    std::thread::sleep(Duration::from_millis(80));
    f.pump(5);
    f.roundtrip(id);
    assert!(
        f.client(id).state.touch_events.contains(&TouchEvent::Down),
        "precondition: the client's wl_touch saw the finger land — a live, \
         server-tracked touch, not a withheld one"
    );

    f.client(id).lock_session();
    f.roundtrip(id);

    assert!(f.state().session_lock.is_locked(), "the lock handler ran");
    assert!(
        !f.state().seat.get_touch().unwrap().is_grabbed(),
        "a touch grab from before the lock must not survive it"
    );

    let events_before_lift = f.client(id).state.touch_events.clone();
    touch_up(&mut f, 0);
    f.roundtrip(id);

    assert_eq!(
        f.client(id).state.touch_events,
        events_before_lift,
        "a lift on a slot that was already down when the session locked must not \
         reach the window it landed on"
    );
}

const TICK: Duration = Duration::from_millis(16);
const PAST_HOLD: Duration = Duration::from_millis(600);
const MAX_TICKS: usize = 600;

/// Canvas-space center of `window`'s current geometry.
fn center_of(
    f: &mut Fixture,
    window: &smithay::desktop::Window,
) -> Point<f64, smithay::utils::Logical> {
    let pos = f.state().stage.position_of(window).unwrap();
    let size = window.geometry().size;
    Point::from((
        pos.x as f64 + size.w as f64 / 2.0,
        pos.y as f64 + size.h as f64 / 2.0,
    ))
}

/// Map a 400×300 window, reset to the identity view, and point the mouse at
/// its center — the setup every "does pointer focus survive lock/unlock"
/// scenario shares. Asserts the precondition that the pointer is focused on
/// the window before returning it.
fn window_under_pointer(f: &mut Fixture, id: ClientId) -> Window {
    map_window(f, id, "a", (400, 300));
    let window = window_by_app_id(f, "a").unwrap();
    origin_view(f);

    let center = center_of(f, &window);
    pointer_to(f, &FakeDevice::mouse(), center);
    assert_eq!(
        pointer_focus(f),
        Some(server_surface(&window)),
        "precondition: the pointer is focused on the window under the cursor"
    );
    window
}

/// A fit's camera pan can still be parked behind a live grab on a *different*
/// window when the session locks — `disarm_interactive_move` (run from that
/// grab's `unset` during `lock()`'s own pointer teardown) lands it straight
/// into `camera_target`. The per-output clear has to be the last word on that
/// state, or the pan survives as a live flight under the lock screen.
///
/// This test never lets the fit's animation converge, so it deliberately ends
/// off the fixture's teardown baseline — `skip_baseline_check` is the
/// documented escape for exactly that.
#[test]
fn lock_does_not_leave_a_deferred_camera_pan_flying() {
    let mut f = Fixture::new();
    f.skip_baseline_check();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    map_window(&mut f, id, "fit-me", (400, 300));
    let fit_target = window_by_app_id(&mut f, "fit-me").unwrap();
    f.state().map_window(
        StageWindow::Client(fit_target.clone()),
        Point::from((0, 0)),
        false,
    );

    // Far outside the fit's near-fullscreen bounds, so fitting "fit-me" can
    // never cover it — the grab below must land on a window the fit does not
    // own, or `end_element_animation`'s same-element exemption would apply the
    // pan immediately instead of deferring it.
    map_window(&mut f, id, "drag-me", (400, 300));
    let dragged = window_by_app_id(&mut f, "drag-me").unwrap();
    f.state().map_window(
        StageWindow::Client(dragged.clone()),
        Point::from((5000, 0)),
        false,
    );

    f.state().with_output_state(|os| {
        os.camera = Point::from((0.0, 0.0));
        os.camera_target = None;
        os.zoom = 1.0;
        os.zoom_target = None;
    });

    let target_id = f.state().stage.id_of(&fit_target).unwrap();
    f.state().fit_window(&fit_target);
    let base = Instant::now();
    f.state().tick_window_animations_at(TICK, base);
    assert!(
        f.state().window_animations.start_held(target_id),
        "precondition: the fit is frozen, waiting on the client's redraw"
    );

    let drag_pos = center_of(&mut f, &dragged);
    assert!(
        f.state().try_start_gesture_move(drag_pos, false),
        "precondition: the move grab installed on the other window"
    );

    let past = base + PAST_HOLD;
    for _ in 0..MAX_TICKS {
        f.state().tick_window_animations_at(TICK, past);
        if !f.state().window_animations.start_held(target_id) {
            break;
        }
    }
    assert!(
        !f.state().window_animations.start_held(target_id),
        "precondition: the freeze budget expired, releasing the fit's pan"
    );
    assert!(
        !f.state().deferred_views.is_empty(),
        "precondition: the released pan is deferred behind the live grab"
    );
    assert!(
        f.state().seat.get_pointer().unwrap().is_grabbed(),
        "precondition: the move grab is still live when the session locks"
    );
    assert!(
        f.state().camera_target().is_none(),
        "precondition: nothing has landed the pan yet"
    );

    // `apply_pending_view` only lands the deferred pan if the viewport hasn't
    // drifted from the camera/zoom it was staged against — without this, a
    // future change to the staging conditions could make the guard reject the
    // pan on its own, and the assertion below would pass whether or not the
    // per-output clear ordering this test targets is even correct.
    let output_name = f.state().active_output().unwrap().name();
    let pending = f.state().deferred_views.get(&output_name).unwrap().clone();
    let (camera, zoom) = f
        .state()
        .with_output_state(|os| (os.camera, os.zoom))
        .unwrap();
    assert_eq!(
        (camera, zoom),
        (pending.staged_camera, pending.staged_zoom),
        "precondition: the deferred pan's camera/zoom guard must still pass at landing time"
    );

    f.client(id).lock_session();
    f.roundtrip(id);

    assert!(f.state().session_lock.is_locked(), "the lock handler ran");
    assert!(
        f.state().camera_target().is_none(),
        "a pan the grab teardown lands during lock() must not survive as a \
         live camera flight under the lock screen"
    );
}

/// A live keyboard grab must not survive `lock()`. The canonical offender is a
/// `zwp_input_method_v2` grab: it forwards every key straight to the IME
/// client and never lets it reach the lock surface, so the password would be
/// typed into the wrong client and the session could never be unlocked from
/// the keyboard. Installed here is smithay's real `InputMethodKeyboardGrab` —
/// the same type production input-method handling installs — but wired
/// directly through `KeyboardHandle::set_grab` rather than through a live
/// `zwp_input_method_v2` client, which the harness doesn't stand up; `lock()`
/// only needs a grab installed to exercise its teardown, and any
/// `KeyboardGrab` impl demonstrates that.
#[test]
fn lock_unsets_a_live_keyboard_grab() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    let keyboard = f.state().seat.get_keyboard().unwrap();
    let serial = smithay::utils::SERIAL_COUNTER.next_serial();
    keyboard.set_grab(f.state(), InputMethodKeyboardGrab::default(), serial);
    assert!(
        keyboard.is_grabbed(),
        "precondition: the keyboard grab is installed"
    );

    f.client(id).lock_session();
    f.roundtrip(id);

    assert!(
        !f.state().seat.get_keyboard().unwrap().is_grabbed(),
        "a live keyboard grab must not survive lock() — otherwise it would swallow \
         every keypress meant for the lock surface's password prompt"
    );
}

/// Drive a client's already-issued lock through to `Locked`: create a lock
/// surface for `output`, ack its configure, attach a buffer sized to match,
/// and commit — the ritual a real lock screen runs on startup. Returns the
/// server-side lock surface.
fn confirm_lock(f: &mut Fixture, id: ClientId, output: &Output) -> WlSurface {
    let wl_output = f.client(id).output(&output.name());
    let surface = f.client(id).create_lock_surface(&wl_output).surface.clone();
    f.roundtrip(id);

    let lock_surface = f.client(id).lock_surface(&surface);
    let (w, h) = lock_surface.configures_received.last().unwrap().1;
    lock_surface.set_size(w, h);
    lock_surface.attach_new_buffer();
    lock_surface.ack_last_and_commit();
    f.double_roundtrip(id);

    f.state()
        .lock_surfaces
        .get(output)
        .unwrap()
        .wl_surface()
        .clone()
}

/// Wait out the real [`PENDING_LOCK_DEADLINE`], pumping the loop so its calloop
/// timer gets a chance to fire. Bounded well past the deadline so a timer that
/// never lands fails the caller's own precondition instead of hanging the suite.
fn run_pending_deadline(f: &mut Fixture) {
    let bound = Instant::now() + PENDING_LOCK_DEADLINE * 5;
    while Instant::now() < bound {
        if !matches!(f.state().session_lock, SessionLock::Pending { .. }) {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
        f.pump(1);
    }
}

/// Kill `dead`'s connection and let a fresh client take over its lock: the
/// replacement locks, creates a lock surface for `output`, and commits it
/// through to `Locked`. Returns the replacement's id and its server-side lock
/// surface.
fn crash_and_replace(f: &mut Fixture, dead: ClientId, output: &Output) -> (ClientId, WlSurface) {
    f.kill_client(dead);
    f.pump(10);

    let replacement = f.add_client();
    f.client(replacement).lock_session();
    f.roundtrip(replacement);
    let surface = confirm_lock(f, replacement, output);
    (replacement, surface)
}

/// A second `lock` from another client while the first is alive must be
/// refused outright: the incumbent keeps the session, and the newcomer only
/// ever sees `finished`, never `locked`. Before this guard, the second lock
/// would displace the first — whose surfaces `lock_surfaces` still held — and
/// could then unlock a session it never earned.
#[test]
fn second_lock_is_refused_while_the_first_client_is_alive() {
    let mut f = Fixture::new();
    // `confirm_lock` populates `lock_surfaces`, and this scenario never
    // unlocks to drain it.
    f.skip_baseline_check();
    let output = f.add_output(1, (1920, 1080));
    let a = f.add_client();
    let b = f.add_client();

    f.client(a).lock_session();
    f.roundtrip(a);
    let a_surface = confirm_lock(&mut f, a, &output);
    assert_eq!(
        keyboard_focus(&mut f),
        Some(a_surface.clone()),
        "precondition: the first client's lock surface holds keyboard focus"
    );

    f.client(b).lock_session();
    f.roundtrip(b);

    assert_eq!(
        f.client(b).lock_events(),
        &[LockEvent::Finished],
        "a second client must be refused outright while the first lock is alive — it \
         may only ever see `finished`, never `locked`"
    );
    assert_eq!(
        keyboard_focus(&mut f),
        Some(a_surface),
        "the original client's lock surface must keep keyboard focus after a refused \
         takeover attempt"
    );
}

/// The crash-recovery path: a locking client dying must not permanently wedge
/// the session — its outputs are blanked with nothing left to drive them and
/// no way back short of a VT switch. A replacement is allowed to take over,
/// and must actually complete its lock, not just get past the refusal guard.
#[test]
fn a_lock_whose_client_died_can_be_replaced() {
    let mut f = Fixture::new();
    // `crash_and_replace` confirms the replacement's lock, populating
    // `lock_surfaces`, and this scenario never unlocks to drain it.
    f.skip_baseline_check();
    let output = f.add_output(1, (1920, 1080));
    let a = f.add_client();

    f.client(a).lock_session();
    f.roundtrip(a);

    let (b, _surface) = crash_and_replace(&mut f, a, &output);

    assert_eq!(
        f.client(b).lock_events(),
        &[LockEvent::Locked],
        "a lock whose client died must be replaceable, and the replacement must \
         actually reach `locked`"
    );
    assert!(
        matches!(f.state().session_lock, SessionLock::Locked { .. }),
        "the session must be `Locked` again once the replacement's surface commits"
    );
}

/// The replacement's lock surface must receive keyboard focus, the same as a
/// first-ever lock does. The takeover deliberately routes through `Pending`
/// rather than confirming immediately: `update_keyboard_focus` bails while
/// locked, so only the `Pending` → `Locked` commit in `CompositorHandler`
/// grants the lock surface the keyboard. An earlier confirmation would still
/// render the lock screen but leave it unable to receive a password.
#[test]
fn replacement_lock_surface_receives_keyboard_focus() {
    let mut f = Fixture::new();
    // `crash_and_replace` confirms the replacement's lock, populating
    // `lock_surfaces`, and this scenario never unlocks to drain it.
    f.skip_baseline_check();
    let output = f.add_output(1, (1920, 1080));
    let a = f.add_client();

    f.client(a).lock_session();
    f.roundtrip(a);

    let (_b, surface) = crash_and_replace(&mut f, a, &output);

    assert_eq!(
        keyboard_focus(&mut f),
        Some(surface),
        "the replacement lock surface must hold keyboard focus once its commit \
         confirms the lock"
    );
}

/// The takeover routes through `Pending`, and `Pending` is what lets the
/// desktop through the render gate so a fresh lock has no blank flash. Here
/// there is nothing to flash: the session was already locked and stays locked
/// throughout. Without the guard the desktop is composited back onto a blanked
/// screen for up to the deadline, with input still dead — at the request of any
/// client that can reach the manager global once the incumbent has died.
#[test]
fn a_replacement_lock_keeps_painting_lock_frames_while_pending() {
    let mut f = Fixture::new();
    let output = f.add_output(1, (1920, 1080));
    let a = f.add_client();

    f.client(a).lock_session();
    f.roundtrip(a);
    confirm_lock(&mut f, a, &output);
    assert!(
        f.state().session_lock.renders_lock_frame(),
        "precondition: the incumbent's lock frames are what the outputs paint"
    );

    f.kill_client(a);
    f.pump(10);

    let b = f.add_client();
    f.client(b).lock_session();
    f.roundtrip(b);

    assert!(
        matches!(f.state().session_lock, SessionLock::Pending { .. }),
        "precondition: the replacement waits in `Pending` for its own lock \
         surface to commit"
    );
    assert!(
        f.state().session_lock.renders_lock_frame(),
        "a replacement lock must keep the outputs on lock frames while it comes \
         up — the session never stopped being locked"
    );
}

/// A lock object can die while its client lives on: smithay posts
/// `invalid_destroy` only once the locker has been consumed, and a `Pending`
/// lock never consumes it — so a client may claim a lock surface on every
/// output, `destroy` its lock cleanly, and stay connected. Nothing sweeps those
/// surfaces (`destroyed` fires on `wl_surface` death, not role death), so
/// without the clear in the takeover arm they outlive the lock they belonged
/// to: on every output the replacement has not reached yet they go on painting,
/// taking locked pointer input, and holding the keyboard, and their keys stall
/// the replacement's own `all_ready` for good.
#[test]
fn a_takeover_drops_the_lock_surfaces_of_the_lock_it_replaces() {
    let mut f = Fixture::new();
    // B's own lock surface stays in `lock_surfaces`, and this scenario never
    // unlocks to drain it.
    f.skip_baseline_check();
    let reached = f.add_output(1, (1920, 1080));
    let unreached = f.add_output(2, (1920, 1080));
    let a = f.add_client();

    f.client(a).lock_session();
    f.roundtrip(a);
    let wl_reached = f.client(a).output(&reached.name());
    f.client(a).create_lock_surface(&wl_reached);
    let wl_unreached = f.client(a).output(&unreached.name());
    f.client(a).create_lock_surface(&wl_unreached);
    f.roundtrip(a);

    let surface_on = |f: &mut Fixture, output: &Output| {
        f.state()
            .lock_surfaces
            .get(output)
            .map(|ls| ls.wl_surface().clone())
    };
    let a_reached = surface_on(&mut f, &reached);
    let a_unreached = surface_on(&mut f, &unreached);
    assert!(
        a_reached.is_some() && a_unreached.is_some(),
        "precondition: A holds a lock surface on both outputs"
    );

    f.client(a).destroy_lock_object();
    f.roundtrip(a);

    assert!(
        matches!(f.state().session_lock, SessionLock::Pending { .. }),
        "precondition: A's lock is still unconfirmed, so the `destroy` was \
         accepted without a protocol error — if the deadline beat it here, the \
         locker was consumed and A has been killed for `invalid_destroy`"
    );
    assert_eq!(
        f.state().lock_surfaces.len(),
        2,
        "precondition: A is still connected and its surfaces still alive, so \
         nothing has swept them"
    );

    let b = f.add_client();
    f.client(b).lock_session();
    f.roundtrip(b);
    assert_eq!(
        f.client(b).lock_events(),
        &[],
        "precondition: B was not refused — A's destroyed lock object let it \
         take the session over"
    );

    // B comes up on one of the two outputs only: it replaces A's surface there
    // and never touches the other.
    let wl_reached = f.client(b).output(&reached.name());
    f.client(b).create_lock_surface(&wl_reached);
    f.roundtrip(b);

    assert!(
        surface_on(&mut f, &reached).is_some_and(|s| Some(s) != a_reached),
        "precondition: B's own surface took the output it reached over from A's"
    );
    assert_eq!(
        surface_on(&mut f, &unreached),
        None,
        "a takeover must drop the replaced lock's surfaces: on an output the \
         replacement has not reached, a surface belonging to a client that \
         merely destroyed its lock would go on painting, taking locked pointer \
         input, and holding the keyboard — and its key would stall `all_ready` \
         for good"
    );
    assert_ne!(
        surface_on(&mut f, &reached),
        a_unreached,
        "sanity: A's other surface was not merely rehomed onto the output B \
         reached"
    );
}

/// Lock the session for a fresh client, leaving its locker unconsumed so the
/// client may later `destroy` its lock object cleanly instead of being killed
/// for `invalid_destroy`. What leaves it unconsumed is an output still owing a
/// lock frame; `active_outputs` is the udev backend's set, so the fixture has to
/// seed it. Returns the client and its lock surface.
fn lock_leaving_the_locker_unconsumed(f: &mut Fixture) -> (ClientId, WlSurface) {
    let output = f.add_output(1, (1920, 1080));
    f.state().active_outputs.insert(output.clone());
    let a = f.add_client();

    f.client(a).lock_session();
    f.roundtrip(a);
    let surface = confirm_lock(f, a, &output);
    (a, surface)
}

/// `holder` drops its lock but stays connected — so the surface it put on
/// screen is still alive, and still holds whatever focus it was given — and a
/// fresh client takes the session over, landing in `Pending` while it waits for
/// a lock surface of its own.
fn take_over_from(f: &mut Fixture, holder: ClientId) {
    f.client(holder).destroy_lock_object();
    f.roundtrip(holder);

    let b = f.add_client();
    f.client(b).lock_session();
    f.roundtrip(b);
    assert!(
        matches!(f.state().session_lock, SessionLock::Pending { .. }),
        "precondition: the replacement took the session over and waits in \
         `Pending` for its own lock surface"
    );
}

/// Clearing `lock_surfaces` re-aims rendering and pointer and touch routing —
/// all three look the map up by output. Keyboard focus does not live there: it
/// sits on the seat, still on the surface the takeover just evicted, and
/// `Pending` answers `is_locked()`, so nothing re-targets it until the
/// newcomer's own commit reaches `enter_locked`. Every keystroke typed at the
/// new lock screen in between — up to the whole deadline — would reach the
/// client that was replaced.
#[test]
fn a_takeover_takes_the_keyboard_off_the_surface_it_evicts() {
    let mut f = Fixture::new();
    let (a, a_surface) = lock_leaving_the_locker_unconsumed(&mut f);
    assert_eq!(
        keyboard_focus(&mut f),
        Some(a_surface),
        "precondition: A's lock surface holds the keyboard"
    );

    take_over_from(&mut f, a);

    assert_eq!(
        keyboard_focus(&mut f),
        None,
        "a takeover must take the keyboard off the surface it evicts — input \
         stays locked throughout, so that surface would otherwise go on \
         receiving every key meant for the replacement's password prompt"
    );
}

/// The touch half of the same rule. A slot that went down on the evicted lock
/// screen is stored on the seat pointing at that surface, and locked
/// `on_touch_motion`/`on_touch_up` forward with `focus: None` — so membership in
/// `lock_slots` is the only thing deciding whether the finger keeps being
/// delivered. Clearing `lock_surfaces` cannot reach it: nothing re-derives a
/// settled slot's focus from the map. Left alone, the finger goes on stroking
/// the replaced client's password prompt for as long as it stays down.
#[test]
fn a_takeover_stops_an_in_flight_finger_reaching_the_surface_it_evicts() {
    let mut f = Fixture::new();
    let (a, _) = lock_leaving_the_locker_unconsumed(&mut f);

    touch_down(&mut f, Point::from((10.0, 10.0)), 0);
    f.roundtrip(a);
    assert!(
        f.client(a).state.touch_events.contains(&TouchEvent::Down),
        "precondition: the finger landed on A's lock surface, so the seat holds \
         its focus for the rest of the sequence"
    );

    take_over_from(&mut f, a);

    let events_after_takeover = f.client(a).state.touch_events.clone();
    touch_motion(&mut f, Point::from((20.0, 20.0)), 0);
    touch_up(&mut f, 0);
    f.roundtrip(a);

    assert_eq!(
        f.client(a).state.touch_events,
        events_after_takeover,
        "a takeover must disown the fingers that went down on the lock screen it \
         replaces — neither their motion nor their lift may reach it"
    );
}

/// Taking a `Pending` over must schedule a repaint. `keep_lock_frames` only
/// decides what the *next* frame paints, and the `Pending` being replaced may
/// have been showing the desktop — a client that dies inside its own first
/// second, before any lock surface commits. Nothing else redraws a static
/// desktop, so without this the desktop stays on the panel for the whole of the
/// replacement's deadline: exactly the leak the flag exists to close.
#[test]
fn a_takeover_of_a_pending_still_showing_the_desktop_schedules_a_repaint() {
    let mut f = Fixture::new();
    let output = f.add_output(1, (1920, 1080));
    // `mark_all_dirty` copies `active_outputs`, which only the udev backend
    // populates — without this the repaint under test has nothing to schedule.
    f.state().active_outputs.insert(output.clone());
    let a = f.add_client();

    f.client(a).lock_session();
    f.roundtrip(a);
    assert!(
        !f.state().session_lock.renders_lock_frame(),
        "precondition: the fresh lock left the desktop up for its wait"
    );

    f.kill_client(a);
    f.pump(10);

    let b = f.add_client();
    // The takeover has to be from `Pending`: `mark_all_dirty` runs in that arm
    // either way, so a deadline that fired in between would leave this passing
    // without ever testing the case it is named for.
    assert!(
        matches!(f.state().session_lock, SessionLock::Pending { .. }),
        "precondition: A's lock is still `Pending` when B takes over"
    );
    f.state().redraws_needed.clear();
    f.client(b).lock_session();
    f.roundtrip(b);

    assert!(
        f.state().session_lock.renders_lock_frame(),
        "precondition: the takeover switched the outputs to lock frames"
    );
    assert!(
        f.state().redraws_needed.contains(&output),
        "a takeover must schedule the frame that puts the lock frame on screen \
         — otherwise the desktop the replaced `Pending` was showing sits there \
         for the whole deadline"
    );
}

/// A lock arriving while DPMS has a panel dark must keep the outputs on lock
/// frames for its `Pending` wait. There is no desktop on a dark panel to flash
/// away, and the first stray input would otherwise light it — input runs
/// `wake_dpms_off_outputs` ahead of the locked-input gate — onto the live
/// desktop this window is still showing. The stock idle setup lands in exactly
/// this state: blank at 300s, lock at 600s.
#[test]
fn a_lock_arriving_on_a_dark_panel_keeps_lock_frames_while_pending() {
    let mut f = Fixture::new();
    let output = f.add_output(1, (1920, 1080));
    // Requested and already drained by the render loop: the panel is dark.
    f.state().dpms_off_outputs.insert(output.clone());
    let id = f.add_client();

    f.client(id).lock_session();
    f.roundtrip(id);

    assert!(
        matches!(f.state().session_lock, SessionLock::Pending { .. }),
        "precondition: no lock surface has committed, so the lock is still `Pending`"
    );
    assert!(
        f.state().session_lock.renders_lock_frame(),
        "a lock requested while a panel is already dark must not put the \
         desktop back on it for the pending wait"
    );
}

/// A change detector, deliberately: `lock` never reads `pending_dpms`, so this
/// runs the identical production path as
/// [`a_lock_arriving_on_a_dark_panel_keeps_lock_frames_while_pending`] and pins
/// the predicate to `dpms_off_outputs` against a later switch to
/// `confirmed_dark`, which would exclude exactly this state. That exclusion
/// would be wrong: a queued DPMS-off means the panel is still lit, and the input
/// that would wake a dark one cancels the queued off outright — either way the
/// desktop `Pending` is painting ends up on a lit panel.
#[test]
fn a_lock_arriving_with_a_dpms_off_still_queued_keeps_lock_frames_too() {
    let mut f = Fixture::new();
    let output = f.add_output(1, (1920, 1080));
    f.state().dpms_off_outputs.insert(output.clone());
    f.state().pending_dpms.insert(output.clone(), false);
    let id = f.add_client();

    f.client(id).lock_session();
    f.roundtrip(id);

    assert!(
        matches!(f.state().session_lock, SessionLock::Pending { .. }),
        "precondition: no lock surface has committed, so the lock is still `Pending`"
    );
    assert!(
        f.state().session_lock.renders_lock_frame(),
        "an output whose DPMS-off has not drained yet must be covered too — \
         input cancels its queued off and leaves the panel lit"
    );
}

/// `lock` reads the DPMS state once, at lock time, so an output going dark
/// *inside* the pending wait has to flip the flag itself. Without it the desktop
/// goes on being painted under a panel the user's next input re-lights —
/// `wake_dpms_off_outputs` runs ahead of the locked-input gate — which is the
/// leak the DPMS case exists to close, reopened by a few hundred milliseconds of
/// timing.
///
/// Covers the helper only. `set_dpms` returns early without a seat session and
/// no fixture has one, so the call in its off branch is what actually arms this
/// in production and is what this cannot reach: delete that call and this test
/// still passes.
#[test]
fn an_output_going_dark_during_pending_stops_the_desktop_being_painted() {
    let mut f = Fixture::new();
    let output = f.add_output(1, (1920, 1080));
    let id = f.add_client();

    f.client(id).lock_session();
    f.roundtrip(id);
    assert!(
        matches!(f.state().session_lock, SessionLock::Pending { .. }),
        "precondition: no lock surface has committed, so the lock is still `Pending`"
    );
    assert!(
        !f.state().session_lock.renders_lock_frame(),
        "precondition: nothing was dark at lock time, so the desktop is up for \
         the wait"
    );

    // The whole off branch of `set_dpms`, which cannot be driven from here: it
    // returns early without a seat session, and no fixture has one.
    f.state().dpms_off_outputs.insert(output.clone());
    f.state().keep_lock_frames_while_pending();

    assert!(
        f.state().session_lock.renders_lock_frame(),
        "an output going dark mid-wait must stop the desktop being painted — \
         the input that wakes the panel would otherwise light it straight onto \
         whatever this `Pending` is showing"
    );
}

/// `new_surface` must refuse a lock surface from any client other than the
/// one holding the lock — see the comment on that guard for why smithay's own
/// `locked_outputs` check doesn't already cover this.
#[test]
fn new_surface_from_a_refused_client_does_not_overwrite_the_incumbent_lock_surface() {
    let mut f = Fixture::new();
    // `confirm_lock` populates `lock_surfaces`, and this scenario never
    // unlocks to drain it.
    f.skip_baseline_check();
    let output = f.add_output(1, (1920, 1080));
    let a = f.add_client();
    let b = f.add_client();

    f.client(a).lock_session();
    f.roundtrip(a);
    let a_surface = confirm_lock(&mut f, a, &output);

    f.client(b).lock_session();
    f.roundtrip(b);
    assert_eq!(
        f.client(b).lock_events(),
        &[LockEvent::Finished],
        "precondition: b's lock request was refused while a's is alive"
    );

    let wl_output = f.client(b).output(&output.name());
    f.client(b).create_lock_surface(&wl_output);
    f.roundtrip(b);

    assert_eq!(
        f.state()
            .lock_surfaces
            .get(&output)
            .unwrap()
            .wl_surface()
            .clone(),
        a_surface,
        "a refused client's get_lock_surface must not overwrite the incumbent's lock \
         surface"
    );

    pointer_to_screen(&mut f, &FakeDevice::mouse(), Point::from((10.0, 10.0)));
    assert_eq!(
        pointer_focus(&mut f),
        Some(a_surface),
        "locked pointer input must keep routing to the incumbent's lock surface, not \
         the refused client's"
    );
}

/// A pointer resync deferred by `warp_pointer` and flushed on the next
/// rendered frame must not re-target focus at the app behind the lock screen —
/// `focus_under` is lock-unaware, so the gate has to live in the flush itself.
#[test]
fn flush_pointer_resync_does_not_restore_focus_to_the_window_behind_the_lock_screen() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    window_under_pointer(&mut f, id);

    f.client(id).lock_session();
    f.roundtrip(id);
    assert_eq!(
        pointer_focus(&mut f),
        None,
        "precondition: lock() cleared pointer focus"
    );

    f.state().pending_pointer_resync = true;
    f.state().flush_pointer_resync();

    assert_eq!(
        pointer_focus(&mut f),
        None,
        "a resync flushed mid-lock must not restore pointer focus to the app behind \
         the lock screen"
    );
}

/// Fullscreen entered while locked must not move pointer focus onto the
/// fullscreening app — the locked input path sends `button`/`axis` events
/// straight at `current_focus()`, so focusing the app here would hand it every
/// click and scroll (and activate its cursor lock) under the lock screen.
#[test]
fn enter_fullscreen_while_locked_does_not_focus_the_pointer_on_the_window() {
    let mut f = Fixture::new();
    // `enter_fullscreen` moves the output's camera, seeding a per-output blur
    // generation that only clears on output disconnect.
    f.skip_baseline_check();
    let output = f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let window = window_under_pointer(&mut f, id);

    f.client(id).lock_session();
    f.roundtrip(id);
    assert_eq!(
        pointer_focus(&mut f),
        None,
        "precondition: lock() cleared pointer focus"
    );

    f.state().enter_fullscreen(&window, Some(output));

    assert_eq!(
        pointer_focus(&mut f),
        None,
        "entering fullscreen while locked must not hand pointer focus to the app"
    );
}

/// `unlock()` must re-seat pointer focus without the pointer moving — the
/// first click after unlocking has to reach the window under the cursor, not
/// wait for a motion event to notice it's there.
#[test]
fn unlock_reseats_pointer_focus_without_the_pointer_moving() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let window = window_under_pointer(&mut f, id);

    f.client(id).lock_session();
    f.roundtrip(id);
    assert_eq!(
        pointer_focus(&mut f),
        None,
        "precondition: lock() cleared pointer focus"
    );

    f.state().unlock();

    assert_eq!(
        pointer_focus(&mut f),
        Some(server_surface(&window)),
        "unlock() must restore pointer focus to the window under the cursor without \
         waiting for the pointer to move"
    );
}

/// While locked, the stored pointer location is screen coords — the locked
/// motion handlers hand the lock surface screen-space positions — and
/// `unlock` converts it back to canvas coords. A non-origin camera and a
/// non-1.0 zoom make a missing (or backwards) conversion visible; at the
/// identity view canvas and screen coincide and either bug would go unnoticed.
#[test]
fn lock_and_unlock_convert_the_stored_pointer_location_between_screen_and_canvas_space() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    map_window(&mut f, id, "a", (400, 300));
    let window = window_by_app_id(&mut f, "a").unwrap();

    let canvas_center = center_of(&mut f, &window);

    let zoom = 2.0;
    let camera = Point::from((
        canvas_center.x - 1920.0 / 2.0 / zoom,
        canvas_center.y - 1080.0 / 2.0 / zoom,
    ));
    custom_view(&mut f, camera, zoom);

    pointer_to(&mut f, &FakeDevice::mouse(), canvas_center);
    assert_eq!(
        pointer_focus(&mut f),
        Some(server_surface(&window)),
        "precondition: the pointer is focused on the window under the cursor"
    );
    assert_eq!(
        f.state().seat.get_pointer().unwrap().current_location(),
        canvas_center,
        "precondition: the stored location is canvas-space before the lock"
    );

    f.client(id).lock_session();
    f.roundtrip(id);

    let expected_screen = canvas_to_screen(CanvasPos(canvas_center), camera, zoom).0;
    assert_eq!(
        f.state().seat.get_pointer().unwrap().current_location(),
        expected_screen,
        "the stored location must become screen coords while locked"
    );

    f.state().unlock();

    assert_eq!(
        f.state().seat.get_pointer().unwrap().current_location(),
        canvas_center,
        "unlock() must convert the stored location back to canvas space"
    );
}

/// A close armed by a finger on the close button must not survive the lock —
/// otherwise a touch-up after unlocking closes a window whose press predates
/// the lock entirely, hit-tested against stale pre-lock coordinates.
#[test]
fn lock_clears_a_touch_close_armed_before_it() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    origin_view(&mut f);
    let id = f.add_client();

    let surface = map_window(&mut f, id, "a", (400, 300));
    let window = window_by_app_id(&mut f, "a").unwrap();
    // y = 30 leaves room above the window for its title bar to land on-screen
    // — the chrome sits above `window_loc.y`, and a touch device can't report
    // a position off the top of the viewport.
    f.state().map_window(
        StageWindow::Client(window.clone()),
        Point::from((0, 30)),
        false,
    );
    give_ssd(&mut f, &window);

    // Close button, bar 25px tall: x in [400-25-8, 400-8), y in [30-25, 30).
    let close = Point::from((400.0 - 20.0, 30.0 - 12.0));
    touch_down(&mut f, close, 0);
    f.double_roundtrip(id);

    assert!(
        f.state().touch_state.pending_close.is_some(),
        "precondition: the finger landed on the close button and armed a pending close"
    );

    f.client(id).lock_session();
    f.roundtrip(id);

    assert!(
        f.state().touch_state.pending_close.is_none(),
        "a close armed before the lock must not survive it"
    );

    // The lift happens after unlocking, not before — while still locked,
    // `on_touch_up` forwards straight to the lock surface and never even
    // looks at `pending_close`; the stale close would fire on the first
    // lift *after* unlock, which is the scenario this test is about.
    f.state().unlock();
    f.roundtrip(id);
    touch_up(&mut f, 0);
    f.roundtrip(id);

    assert!(
        !f.client(id).window(&surface).close_requested,
        "a touch-up after unlock must not close a window whose armed close predates \
         the lock"
    );
}

/// A deferred single-tap center armed before the lock must not fire mid-lock:
/// its timer outlives the fingers that armed it, and firing would arm
/// `camera_target` under the lock screen via `navigate_to_window`.
#[test]
fn lock_cancels_a_pending_deferred_touch_center() {
    let mut f = Fixture::new();
    f.skip_baseline_check();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    map_window(&mut f, id, "a", (400, 300));
    let window = window_by_app_id(&mut f, "a").unwrap();

    f.state()
        .schedule_pending_center(window.clone(), Duration::from_millis(10));
    assert!(
        f.state().touch_state.pending_center_timer.is_some(),
        "precondition: a deferred center is armed"
    );

    f.client(id).lock_session();
    f.roundtrip(id);

    assert!(
        f.state().touch_state.pending_center_timer.is_none(),
        "a deferred center armed before the lock must not survive it"
    );

    std::thread::sleep(Duration::from_millis(50));
    f.pump(5);

    assert!(
        f.state().camera_target().is_none(),
        "the cancelled timer must not later arm a camera pan mid-lock"
    );
}

/// A hardware `TouchCancel` mid-lock must not touch [`TouchState::lock_slots`]
/// — see the comment on that clear in `lock()` for why only `lock()`/`unlock()`
/// may drain it, not `cancel_touch_sequence`.
///
/// [`TouchState::lock_slots`]: crate::input::touch::TouchState::lock_slots
#[test]
fn a_touch_cancel_mid_lock_does_not_strip_a_post_lock_touch_of_its_up() {
    let mut f = Fixture::new();
    // `confirm_lock` populates `lock_surfaces`, and this scenario never
    // unlocks to drain it.
    f.skip_baseline_check();
    let output = f.add_output(1, (1920, 1080));
    let id = f.add_client();

    f.client(id).lock_session();
    f.roundtrip(id);
    confirm_lock(&mut f, id, &output);

    touch_down(&mut f, Point::from((10.0, 10.0)), 0);
    f.roundtrip(id);
    assert!(
        f.client(id).state.touch_events.contains(&TouchEvent::Down),
        "precondition: the post-lock finger reached the lock surface"
    );

    touch_cancel(&mut f);
    f.roundtrip(id);
    touch_up(&mut f, 0);
    f.roundtrip(id);

    assert!(
        f.client(id).state.touch_events.contains(&TouchEvent::Up),
        "a touch that began after the lock must still receive its up after a \
         hardware TouchCancel mid-lock"
    );
}

/// A pointer hide left by touch before the lock must not survive it — see
/// the `hidden_by_touch` reset in `SessionLockHandler::lock`.
#[test]
fn lock_lifts_a_pointer_hide_left_by_a_touch_before_it() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    touch_down(&mut f, Point::from((10.0, 10.0)), 0);
    assert!(
        f.state().cursor.hidden_by_touch,
        "precondition: the touch hid the pointer"
    );

    f.client(id).lock_session();
    f.roundtrip(id);

    assert!(
        !f.state().cursor.hidden_by_touch,
        "a pointer hide left by a touch before the lock must not survive it"
    );
}

/// Relative motion must clamp to the output bounds while locked — see the
/// clamp in `on_pointer_motion_relative`. Pins the top-left corner.
#[test]
fn locked_relative_motion_clamps_at_the_top_left_of_the_output() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    f.client(id).lock_session();
    f.roundtrip(id);

    pointer_to_screen(&mut f, &FakeDevice::mouse(), Point::from((5.0, 5.0)));
    pointer_relative_motion(&mut f, &FakeDevice::mouse(), Point::from((-500.0, -500.0)));

    assert_eq!(
        f.state().seat.get_pointer().unwrap().current_location(),
        Point::from((0.0, 0.0)),
        "a large negative relative delta must clamp at the output's top-left \
         corner, not carry the cursor off it"
    );
}

/// The bottom-right counterpart of
/// [`locked_relative_motion_clamps_at_the_top_left_of_the_output`].
#[test]
fn locked_relative_motion_clamps_at_the_bottom_right_of_the_output() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    f.client(id).lock_session();
    f.roundtrip(id);

    pointer_to_screen(&mut f, &FakeDevice::mouse(), Point::from((1900.0, 1070.0)));
    pointer_relative_motion(&mut f, &FakeDevice::mouse(), Point::from((500.0, 500.0)));

    assert_eq!(
        f.state().seat.get_pointer().unwrap().current_location(),
        Point::from((1919.0, 1079.0)),
        "a large positive relative delta must clamp at the output's \
         bottom-right corner, not carry the cursor off it"
    );
}

/// An in-bounds relative delta must land exactly, not merely stay within
/// bounds — the two clamp tests above only pin the boundaries, which a clamp
/// that also mangled ordinary in-range motion would still pass.
#[test]
fn locked_relative_motion_lands_exactly_when_in_bounds() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    f.client(id).lock_session();
    f.roundtrip(id);

    pointer_to_screen(&mut f, &FakeDevice::mouse(), Point::from((5.0, 5.0)));
    pointer_relative_motion(&mut f, &FakeDevice::mouse(), Point::from((10.0, 10.0)));

    assert_eq!(
        f.state().seat.get_pointer().unwrap().current_location(),
        Point::from((15.0, 15.0)),
        "an in-bounds relative delta must land exactly"
    );
}

/// A finger already down when the lock engages is disowned (absent from
/// `lock_slots`) — motion on it must not re-hide the pointer. See the gate
/// in `on_touch_motion`.
#[test]
fn locked_touch_motion_on_a_disowned_finger_does_not_re_hide_the_pointer() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    touch_down(&mut f, Point::from((10.0, 10.0)), 0);

    f.client(id).lock_session();
    f.roundtrip(id);
    assert!(
        !f.state().cursor.hidden_by_touch,
        "precondition: lock() lifted the hide left by the pre-lock touch"
    );

    touch_motion(&mut f, Point::from((20.0, 20.0)), 0);

    assert!(
        !f.state().cursor.hidden_by_touch,
        "motion from a finger the lock disowned must not re-hide the pointer"
    );
}

/// The other half of the rule under
/// [`locked_touch_motion_on_a_disowned_finger_does_not_re_hide_the_pointer`].
/// Pinned so a future simplification to a blanket `!is_locked()` gate fails
/// loudly — this one may already hold before the disowned-finger fix, since
/// `on_touch_down` alone hides the pointer; it is a characterization test,
/// not a regression test.
#[test]
fn locked_touch_motion_on_a_finger_that_owns_its_slot_still_hides_the_pointer() {
    let mut f = Fixture::new();
    // `confirm_lock` populates `lock_surfaces`, and this scenario never
    // unlocks to drain it.
    f.skip_baseline_check();
    let output = f.add_output(1, (1920, 1080));
    let id = f.add_client();

    f.client(id).lock_session();
    f.roundtrip(id);
    confirm_lock(&mut f, id, &output);

    touch_down(&mut f, Point::from((10.0, 10.0)), 0);
    // Isolate what motion does, independent of `on_touch_down` also setting
    // this unconditionally.
    f.state().cursor.hidden_by_touch = false;

    touch_motion(&mut f, Point::from((20.0, 20.0)), 0);

    assert!(
        f.state().cursor.hidden_by_touch,
        "a finger that went down on the lock screen must still hide the pointer on motion"
    );
}

/// Uses a survivor *smaller* than the disconnected output so a missing
/// clamp is visible: the stored screen-space coordinates would otherwise
/// land out of the survivor's bounds.
#[test]
fn output_disconnect_while_locked_clamps_the_pointer_into_the_survivor() {
    let mut f = Fixture::new();
    let out1 = f.add_output(1, (1920, 1080));
    let out2 = f.add_output(2, (800, 600));
    let id = f.add_client();

    f.state().focused_output = Some(out1.clone());
    f.client(id).lock_session();
    f.roundtrip(id);

    pointer_to_screen(&mut f, &FakeDevice::mouse(), Point::from((1500.0, 900.0)));
    assert_eq!(
        f.state().seat.get_pointer().unwrap().current_location(),
        Point::from((1500.0, 900.0)),
        "precondition: the pointer sits where the disconnected output put it, \
         out of the survivor's bounds"
    );

    f.remove_output(&out1);

    assert_eq!(
        f.state().focused_output.as_ref().map(|o| o.name()),
        Some(out2.name()),
        "precondition: focus moved to the smaller survivor"
    );
    let pointer = f.state().seat.get_pointer().unwrap().current_location();
    assert_eq!(
        (pointer.x, pointer.y),
        (799.0, 599.0),
        "the disconnect must clamp the stored screen-space pointer into the \
         smaller survivor, not leave coordinates that only made sense on the \
         output that just left"
    );
}

/// CHARACTERIZATION, not a regression test: it passes whether or not the
/// render-gate confirmation exists at all, because `active_outputs` is only
/// ever populated by the udev backend, so under the fixture the wait set is
/// always empty and confirmation is always immediate. Recorded so a future
/// change that starts populating `active_outputs` outside udev doesn't
/// silently stall every fixture test that locks.
#[test]
fn locked_arrives_on_the_lock_surfaces_first_commit_with_no_active_outputs() {
    let mut f = Fixture::new();
    // `confirm_lock` populates `lock_surfaces`, and this scenario never
    // unlocks to drain it.
    f.skip_baseline_check();
    let output = f.add_output(1, (1920, 1080));
    let id = f.add_client();

    f.client(id).lock_session();
    f.roundtrip(id);
    confirm_lock(&mut f, id, &output);

    assert_eq!(
        f.client(id).lock_events(),
        &[LockEvent::Locked],
        "characterization: with no active outputs the wait set is always \
         empty, so locked still arrives on the lock surface's first commit"
    );
}

/// The protocol forbids `locked` until a lock frame has been presented on
/// every output — an active output must hold the event back until it reports
/// one in, not confirm on the strength of the commit alone.
#[test]
fn locked_is_withheld_until_the_sole_active_output_presents_a_lock_frame() {
    let mut f = Fixture::new();
    f.skip_baseline_check();
    let output = f.add_output(1, (1920, 1080));
    f.state().active_outputs.insert(output.clone());
    let id = f.add_client();

    f.client(id).lock_session();
    f.roundtrip(id);
    confirm_lock(&mut f, id, &output);

    assert_eq!(
        f.client(id).lock_events(),
        &[],
        "locked must be withheld while the sole active output still owes a \
         presented lock frame"
    );

    f.state().stop_awaiting_lock_frame(&output);
    f.roundtrip(id);

    assert_eq!(
        f.client(id).lock_events(),
        &[LockEvent::Locked],
        "locked arrives once the sole awaited output reports its lock frame \
         presented"
    );
}

/// The counterpart of
/// [`locked_is_withheld_until_the_sole_active_output_presents_a_lock_frame`]
/// with two outputs: one reporting in must not be mistaken for all of them.
#[test]
fn locked_is_withheld_until_every_active_output_presents_a_lock_frame() {
    let mut f = Fixture::new();
    f.skip_baseline_check();
    let out1 = f.add_output(1, (1920, 1080));
    let out2 = f.add_output(2, (1920, 1080));
    f.state().active_outputs.insert(out1.clone());
    f.state().active_outputs.insert(out2.clone());
    let id = f.add_client();

    f.client(id).lock_session();
    f.roundtrip(id);
    confirm_lock(&mut f, id, &out1);

    f.state().stop_awaiting_lock_frame(&out1);
    f.roundtrip(id);
    assert_eq!(
        f.client(id).lock_events(),
        &[],
        "one of two active outputs presenting must not confirm the lock on \
         its own"
    );

    f.state().stop_awaiting_lock_frame(&out2);
    f.roundtrip(id);
    assert_eq!(
        f.client(id).lock_events(),
        &[LockEvent::Locked],
        "locked arrives only once every active output has presented a lock \
         frame"
    );
}

/// Disconnecting one awaited output must not strand the lock behind an
/// output that no longer exists — only the survivor's own presented frame
/// should be left to confirm it.
#[test]
fn disconnecting_an_awaited_output_does_not_block_confirmation_on_the_survivor() {
    let mut f = Fixture::new();
    f.skip_baseline_check();
    let leaving = f.add_output(1, (1920, 1080));
    let staying = f.add_output(2, (1920, 1080));
    f.state().active_outputs.insert(leaving.clone());
    f.state().active_outputs.insert(staying.clone());
    let id = f.add_client();

    f.client(id).lock_session();
    f.roundtrip(id);
    confirm_lock(&mut f, id, &leaving);

    assert!(
        f.state().is_awaiting_lock_frame(&leaving) && f.state().is_awaiting_lock_frame(&staying),
        "precondition: both active outputs are in the wait set"
    );

    f.remove_output(&leaving);

    // Assert on the wait set directly rather than only on the eventual
    // `locked` — `Fixture::remove_output` never touches `active_outputs`, so
    // an implementation that re-derives the wait set from it on every check,
    // instead of maintaining `awaiting_present` and hooking the disconnect,
    // could still reach `locked` correctly here for the wrong reason.
    assert!(
        !f.state().is_awaiting_lock_frame(&leaving),
        "a disconnected output must drop out of the wait set immediately"
    );
    assert!(
        f.state().is_awaiting_lock_frame(&staying),
        "the surviving output must remain awaited — only its own presented \
         frame may confirm the lock"
    );

    f.state().stop_awaiting_lock_frame(&staying);
    f.roundtrip(id);

    assert_eq!(
        f.client(id).lock_events(),
        &[LockEvent::Locked],
        "the survivor's presented lock frame must confirm the lock once the \
         other awaited output has disconnected"
    );
}

/// The keyboard-focus handoff in `CompositorHandler` hangs off entering
/// `Locked`, not off confirmation — a first commit must switch out of
/// `Pending` (and hand the lock surface the keyboard) even while an active
/// output still owes a presented lock frame and `locked` hasn't gone out.
#[test]
fn locked_is_entered_and_keyboard_focus_handed_over_before_confirmation() {
    let mut f = Fixture::new();
    f.skip_baseline_check();
    let output = f.add_output(1, (1920, 1080));
    f.state().active_outputs.insert(output.clone());
    let id = f.add_client();

    f.client(id).lock_session();
    f.roundtrip(id);
    let surface = confirm_lock(&mut f, id, &output);

    assert!(
        matches!(f.state().session_lock, SessionLock::Locked { .. }),
        "the lock surface's first commit must enter Locked even though the \
         output hasn't presented a lock frame yet"
    );
    assert_eq!(
        f.client(id).lock_events(),
        &[],
        "precondition: locked has not been confirmed yet"
    );
    assert_eq!(
        keyboard_focus(&mut f),
        Some(surface),
        "the keyboard-focus handoff hangs off entering Locked, not off \
         confirmation, and must not wait for the output to present"
    );
}

/// The `Pending` window accumulates lock-surface commits, and must let every
/// other surface's commit through untouched: an ordinary toplevel making its
/// first commit in there still owes an initial configure, and a client that
/// never gets one waits for it forever — it may not attach a buffer until the
/// configure it acks arrives.
#[test]
fn a_toplevel_committing_during_pending_still_gets_its_initial_configure() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    f.client(id).lock_session();
    f.roundtrip(id);
    assert!(
        matches!(f.state().session_lock, SessionLock::Pending { .. }),
        "precondition: no lock surface has committed, so the lock is still Pending"
    );

    let window = f.client(id).create_window();
    let surface = window.surface.clone();
    window.set_app_id("late");
    window.commit();
    f.double_roundtrip(id);

    assert!(
        !f.client(id).window(&surface).configures_received.is_empty(),
        "a toplevel's first commit during a pending lock must still be answered \
         with its initial configure — without one the client hangs forever"
    );

    let window = f.client(id).window(&surface);
    window.set_size(400, 300);
    window.attach_new_buffer();
    window.ack_last_and_commit();
    f.double_roundtrip(id);

    assert!(
        window_by_app_id(&mut f, "late").is_some(),
        "and its buffer commit must go on to map the window"
    );
}

/// The `Pending` deadline can reach `Locked` before the client has made a
/// single lock surface, and the accumulate-commits arm no longer matches once
/// it has — so a lock surface created afterwards must still be handed the
/// keyboard on its first commit. Without that handoff focus stays `None` for
/// the rest of the lock: `update_keyboard_focus` bails while locked, so every
/// keystroke is dropped and the session can never be unlocked from the
/// keyboard.
#[test]
fn a_lock_surface_created_after_the_pending_deadline_receives_keyboard_focus() {
    let mut f = Fixture::new();
    // `confirm_lock` populates `lock_surfaces`, and this scenario never
    // unlocks to drain it.
    f.skip_baseline_check();
    let output = f.add_output(1, (1920, 1080));
    let id = f.add_client();

    f.client(id).lock_session();
    f.roundtrip(id);

    run_pending_deadline(&mut f);
    assert!(
        matches!(f.state().session_lock, SessionLock::Locked { .. }),
        "precondition: the deadline forced `Locked` with no lock surface committed"
    );
    assert_eq!(
        keyboard_focus(&mut f),
        None,
        "precondition: there was no lock surface to hand the keyboard to when the \
         deadline fired"
    );

    let surface = confirm_lock(&mut f, id, &output);

    assert_eq!(
        keyboard_focus(&mut f),
        Some(surface),
        "a lock surface created after the deadline forced `Locked` must still \
         receive keyboard focus on its first commit"
    );
}

/// `lock_confirm_timer` is a single field, not keyed by which client armed
/// it. A's commit arms a timer for A's lock; A's client then dies and B
/// replaces it. B's own commit must claim a fresh slot in that field rather
/// than leaving A's registered token sitting there.
///
/// Structural only: it asserts the token is replaced and that B stays awaiting
/// until its own output reports in. It does *not* exercise the backstop firing
/// — the fixture has no seat session, so the timer's blanking is inert whatever
/// token is registered, and the callback's identity check needs the TTY smoke.
#[test]
fn a_dead_clients_confirm_timer_does_not_confirm_the_replacement_lock() {
    let mut f = Fixture::new();
    f.skip_baseline_check();
    let output = f.add_output(1, (1920, 1080));
    f.state().active_outputs.insert(output.clone());
    let a = f.add_client();

    f.client(a).lock_session();
    f.roundtrip(a);
    confirm_lock(&mut f, a, &output);
    assert_eq!(
        f.client(a).lock_events(),
        &[],
        "precondition: A's lock is still awaiting the output's presented frame"
    );
    let a_timer = f.state().lock_confirm_timer;
    assert!(
        a_timer.is_some(),
        "precondition: A's own confirmation timer is armed"
    );

    let (b, _surface) = crash_and_replace(&mut f, a, &output);
    assert_eq!(
        f.client(b).lock_events(),
        &[],
        "precondition: B's replacement lock is likewise still awaiting a \
         presented frame"
    );
    assert_ne!(
        f.state().lock_confirm_timer,
        a_timer,
        "B's commit must arm its own confirmation timer, not leave A's \
         registered token sitting in the slot"
    );

    f.pump(10);
    assert_eq!(
        f.client(b).lock_events(),
        &[],
        "B's lock must not confirm before B's own output has presented a frame"
    );

    f.state().stop_awaiting_lock_frame(&output);
    f.roundtrip(b);
    assert_eq!(
        f.client(b).lock_events(),
        &[LockEvent::Locked],
        "B's lock must still confirm normally once its own output presents"
    );
}

/// `confirmed_dark` must not trust `dpms_off_outputs` alone: it is written at
/// request time, while the render loop's `compositor.clear()` that actually
/// darkens the panel happens later — an output whose DPMS-off is still
/// pending may still be lit and showing the desktop.
#[test]
fn confirmed_dark_output_is_excluded_but_a_pending_dpms_off_output_is_still_awaited() {
    let mut f = Fixture::new();
    f.skip_baseline_check();
    let dark = f.add_output(1, (1920, 1080));
    let dimming = f.add_output(2, (1920, 1080));
    f.state().active_outputs.insert(dark.clone());
    f.state().active_outputs.insert(dimming.clone());

    // `dark`: DPMS-off was requested and the render loop already drained it —
    // the panel is genuinely black.
    f.state().dpms_off_outputs.insert(dark.clone());
    // `dimming`: DPMS-off was requested but the render loop hasn't drained it
    // yet — the panel is still lit and may still show the desktop.
    f.state().dpms_off_outputs.insert(dimming.clone());
    f.state().pending_dpms.insert(dimming.clone(), false);

    let id = f.add_client();
    f.client(id).lock_session();
    f.roundtrip(id);
    confirm_lock(&mut f, id, &dimming);

    assert!(
        !f.state().is_awaiting_lock_frame(&dark),
        "a confirmed-dark output must be excluded from the wait set"
    );
    assert!(
        f.state().is_awaiting_lock_frame(&dimming),
        "an output whose DPMS-off is still pending (not yet drained) must \
         stay in the wait set — the panel may still be lit"
    );
}

/// Lock, confirm, and unlock over the wire (`unlock_and_destroy`) — stopping
/// short of the lock surfaces' own destroy, which a real lock screen sends an
/// event-loop turn later, so commits can land in between.
fn confirm_and_unlock(f: &mut Fixture, id: ClientId, output: &Output) {
    f.client(id).lock_session();
    f.roundtrip(id);
    confirm_lock(f, id, output);

    f.client(id).unlock_session();
    f.roundtrip(id);
    assert!(
        !f.state().session_lock.is_locked(),
        "precondition: the session is unlocked before the role is torn down"
    );
}

/// The full teardown a real lock screen runs: [`confirm_and_unlock`], then the
/// role's own destroy with its `wl_surface` kept — what leaves smithay's
/// never-removed pre-commit hook pointing at a dead proxy. The stale role stays
/// the client's `last_lock_surface`, ready to be committed on.
fn orphan_the_role(f: &mut Fixture, id: ClientId, output: &Output) {
    confirm_and_unlock(f, id, output);

    f.client(id).last_lock_surface().destroy_role();
    f.double_roundtrip(id);
}

/// Push the commit the compositor is expected to reject and assert on the error
/// it posts back. Both the interface and the code, since the codes collide
/// across the interfaces in play — `commit_before_first_ack` and
/// `invalid_destroy` are both 0.
///
/// The error kills `id`, and its `WaylandSource` stays errored in the fixture's
/// outer loop, where the next `Fixture::roundtrip` — for any client at all —
/// would fire its callback and panic. So the client is unregistered here.
fn expect_lock_surface_error(
    f: &mut Fixture,
    id: ClientId,
    expected: ext_session_lock_surface_v1::Error,
    why: &str,
) {
    f.client(id).flush();
    f.pump(10);

    let error = f
        .client(id)
        .protocol_error()
        .unwrap_or_else(|| panic!("{why}"));
    assert_eq!(
        error.object_interface, "ext_session_lock_surface_v1",
        "{why}"
    );
    assert_eq!(error.code, expected as u32, "{why}, got: {}", error.message);

    f.kill_client(id);
}

/// The teardown a real lock screen runs: `unlock_and_destroy`, then — an
/// event-loop turn later — the lock surfaces' own destroy, then a commit on the
/// `wl_surface` it kept. smithay adds a pre-commit hook when the
/// `ext_session_lock_surface_v1` role is created and never removes it, and the
/// role's `destroyed` resets `last_acked` to `None` while leaving the dead
/// proxy in the attributes. So the hook's first and unconditional check fails
/// on every later commit and posts `commit_before_first_ack` on an object that
/// is already gone.
///
/// Without the fix, this doesn't fail on a caught protocol error —
/// `post_error` on a destroyed proxy is never serialized (its id is already
/// gone from the wire's object map), so the client only sees the socket EOF.
/// That surfaces as `Client::dispatch`'s `result.unwrap()`
/// (`src/tests/client.rs`) panicking with a bare `Broken pipe (os error 32)` —
/// not a `protocol_error()`, which maps `Io(_)` to `None` and so never sees it
/// either way. Don't assert on it.
#[test]
fn an_orphaned_lock_commit_does_not_kill_the_client() {
    let mut f = Fixture::new();
    let output = f.add_output(1, (1920, 1080));
    let id = f.add_client();

    orphan_the_role(&mut f, id, &output);

    f.client(id).last_lock_surface().commit();
    f.double_roundtrip(id);

    // The client survives: map an unrelated plain toplevel afterwards and
    // confirm it actually reaches the compositor, not just that the call
    // returned.
    map_window(&mut f, id, "still-alive", (400, 300));
    assert!(
        window_by_app_id(&mut f, "still-alive").is_some(),
        "the client must survive the orphaned lock-surface commit"
    );
}

/// The same teardown with the null-buffer attach Qt ≥ 6.9 emits when it resets
/// a surface's role (`attach(nullptr); commit();`). This one has a second way
/// to die even once the ack check is answered: an orphaned commit carrying
/// `BufferAssignment::Removed` trips the hook's `null_buffer` error instead.
/// See [`an_orphaned_lock_commit_does_not_kill_the_client`] for why the failure
/// arrives as a `Broken pipe` panic rather than a protocol error.
#[test]
fn an_orphaned_lock_commit_with_a_null_buffer_does_not_kill_the_client() {
    let mut f = Fixture::new();
    let output = f.add_output(1, (1920, 1080));
    let id = f.add_client();

    orphan_the_role(&mut f, id, &output);

    let lock_surface = f.client(id).last_lock_surface();
    lock_surface.attach_null();
    lock_surface.commit();
    f.double_roundtrip(id);

    map_window(&mut f, id, "still-alive", (400, 300));
    assert!(
        window_by_app_id(&mut f, "still-alive").is_some(),
        "the client must survive the orphaned lock-surface commit that detaches \
         its buffer"
    );
}

/// The same teardown again, this time with a buffer still attached — the frame
/// a client had queued when it decided to drop the role. The buffer is what
/// carries the commit past the hook's `None` arm into the dimensions check, so
/// it exercises a different tail of the hook than the two above.
/// See [`an_orphaned_lock_commit_does_not_kill_the_client`] for why the failure
/// arrives as a `Broken pipe` panic rather than a protocol error.
#[test]
fn an_orphaned_lock_commit_with_a_buffer_does_not_kill_the_client() {
    let mut f = Fixture::new();
    let output = f.add_output(1, (1920, 1080));
    let id = f.add_client();

    orphan_the_role(&mut f, id, &output);

    let lock_surface = f.client(id).last_lock_surface();
    lock_surface.set_size(1920, 1080);
    lock_surface.attach_new_buffer();
    lock_surface.commit();
    f.double_roundtrip(id);

    map_window(&mut f, id, "still-alive", (400, 300));
    assert!(
        window_by_app_id(&mut f, "still-alive").is_some(),
        "the client must survive the orphaned lock-surface commit that carries a \
         buffer"
    );
}

/// Answering the first orphaned commit erases the shape that identified it: a
/// written `last_acked` reads exactly like a live, acked role. Nothing stops a
/// client committing on the surface again — Qt's role reset detaches the buffer
/// and commits, and a repaint loop may keep going — and a second commit read as
/// live walks into the null-buffer check instead. So the verdict has to be
/// remembered, not re-derived.
/// See [`an_orphaned_lock_commit_does_not_kill_the_client`] for why the failure
/// arrives as a `Broken pipe` panic rather than a protocol error.
#[test]
fn a_second_orphaned_lock_commit_does_not_kill_the_client_either() {
    let mut f = Fixture::new();
    let output = f.add_output(1, (1920, 1080));
    let id = f.add_client();

    orphan_the_role(&mut f, id, &output);

    f.client(id).last_lock_surface().commit();
    f.double_roundtrip(id);

    let lock_surface = f.client(id).last_lock_surface();
    lock_surface.attach_null();
    lock_surface.commit();
    f.double_roundtrip(id);

    map_window(&mut f, id, "still-alive", (400, 300));
    assert!(
        window_by_app_id(&mut f, "still-alive").is_some(),
        "the client must survive a second orphaned commit, whose state no longer \
         looks orphaned"
    );
}

/// The unlock is incidental: what kills the client is the destroyed role plus
/// the hook smithay left behind, and a lock screen that retires one prompt for
/// another does that mid-lock, while driftwm still owns the entry. This is why
/// the orphan has to be recognised from the state a destroyed role leaves —
/// no disown site of driftwm's own observes this one at all.
/// See [`an_orphaned_lock_commit_does_not_kill_the_client`] for why the failure
/// arrives as a `Broken pipe` panic rather than a protocol error.
#[test]
fn an_orphaned_lock_commit_mid_lock_does_not_kill_the_client() {
    let mut f = Fixture::new();
    // The session is still locked at teardown, so `lock_surfaces` never drains.
    f.skip_baseline_check();
    let output = f.add_output(1, (1920, 1080));
    let id = f.add_client();

    f.client(id).lock_session();
    f.roundtrip(id);
    confirm_lock(&mut f, id, &output);
    assert!(
        f.state().session_lock.is_locked(),
        "precondition: the session stays locked across the role's destroy"
    );

    f.client(id).last_lock_surface().destroy_role();
    f.double_roundtrip(id);

    f.client(id).last_lock_surface().commit();
    f.double_roundtrip(id);

    // The client survives: the roundtrips inside `map_window` are the probe —
    // they panic on the socket a killed client leaves behind. Where the toplevel
    // ends up is a separate policy question, and asserting it here would fail
    // this test for an unrelated reason.
    map_window(&mut f, id, "still-alive", (400, 300));
}

/// The suppression has to stay narrow. A role driftwm configured and the client
/// has not acked is *live* — committing on it is a real protocol violation, and
/// smithay must be left to punish it. Only the shape a destroyed role leaves
/// behind may be neutralised, and this one still holds the configure it was
/// sent.
///
/// Unlike the orphan scenarios, the error here lands on a live proxy, so it is
/// serialized and decodable — but it still kills the client, and every dispatch
/// of it from then on panics. Hence [`expect_lock_surface_error`] instead of a
/// roundtrip.
#[test]
fn a_live_lock_role_still_errors_on_a_commit_before_its_first_ack() {
    let mut f = Fixture::new();
    let output = f.add_output(1, (1920, 1080));
    let id = f.add_client();

    f.client(id).lock_session();
    f.roundtrip(id);
    let wl_output = f.client(id).output(&output.name());
    f.client(id).create_lock_surface(&wl_output);
    f.roundtrip(id);

    f.client(id).last_lock_surface().commit();
    expect_lock_surface_error(
        &mut f,
        id,
        ext_session_lock_surface_v1::Error::CommitBeforeFirstAck,
        "a live lock role must still be told it committed before acking its \
         first configure",
    );
}

/// The shape a destroyed role leaves — no pending configure and no ack — is
/// also the shape of a role driftwm declined to configure: `new_surface`
/// returns early for a client that does not hold the lock, and smithay's own
/// initial configure is a no-op with no server-pending state behind it. That
/// role is live, so its commits must still reach the client as real errors —
/// which is the whole job of the gate's "driftwm configured this" half.
#[test]
fn a_lock_role_driftwm_declined_to_configure_still_errors_on_a_commit() {
    let mut f = Fixture::new();
    // The incumbent's lock is still up at teardown, so `lock_surfaces` never
    // drains.
    f.skip_baseline_check();
    let output = f.add_output(1, (1920, 1080));
    let incumbent = f.add_client();
    let refused = f.add_client();

    f.client(incumbent).lock_session();
    f.roundtrip(incumbent);
    confirm_lock(&mut f, incumbent, &output);

    f.client(refused).lock_session();
    f.roundtrip(refused);
    let wl_output = f.client(refused).output(&output.name());
    f.client(refused).create_lock_surface(&wl_output);
    f.roundtrip(refused);
    assert!(
        f.client(refused)
            .last_lock_surface()
            .configures_received
            .is_empty(),
        "precondition: driftwm declined to configure the refused client's role"
    );

    f.client(refused).last_lock_surface().commit();
    expect_lock_surface_error(
        &mut f,
        refused,
        ext_session_lock_surface_v1::Error::CommitBeforeFirstAck,
        "an unconfigured but live role must still get smithay's real error",
    );
}

/// Between `unlock_and_destroy` and the role's own destroy the role is
/// genuinely alive, and commits on it must keep being validated — a marker set
/// at unlock would have swallowed them. Shown with a buffer that contradicts
/// the acked configure, which only a live, unsuppressed role rejects.
#[test]
fn a_lock_commit_between_unlock_and_role_destroy_is_still_validated() {
    let mut f = Fixture::new();
    let output = f.add_output(1, (1920, 1080));
    let id = f.add_client();

    confirm_and_unlock(&mut f, id, &output);

    let lock_surface = f.client(id).last_lock_surface();
    lock_surface.set_size(640, 480);
    lock_surface.attach_new_buffer();
    lock_surface.commit();
    expect_lock_surface_error(
        &mut f,
        id,
        ext_session_lock_surface_v1::Error::DimensionsMismatch,
        "the still-live role must reject a buffer that contradicts its acked size",
    );
}

/// The repair's own poison: answering the orphaned commit means writing a
/// `last_acked` the client never acked, and `get_lock_surface` only repoints
/// the role's proxy — it resets nothing. Left in place, that synthetic would
/// tell the *next* role's first commit it had already acked, masking a genuine
/// violation and feeding the state that decides whether to configure at all.
#[test]
fn a_lock_role_retaken_after_an_orphaned_commit_is_configured_and_validated_afresh() {
    let mut f = Fixture::new();
    let output = f.add_output(1, (1920, 1080));
    let id = f.add_client();

    orphan_the_role(&mut f, id, &output);

    // The commit that writes the synthetic ack.
    f.client(id).last_lock_surface().commit();
    f.double_roundtrip(id);

    let surface = f.client(id).last_lock_surface().surface.clone();
    f.client(id).lock_session();
    f.roundtrip(id);
    let wl_output = f.client(id).output(&output.name());
    f.client(id).retake_lock_surface(&surface, &wl_output);
    f.double_roundtrip(id);

    assert_eq!(
        f.client(id)
            .last_lock_surface()
            .configures_received
            .last()
            .map(|(_, size)| *size),
        Some((1920, 1080)),
        "the retaken role must get an initial configure in its own right"
    );

    f.client(id).last_lock_surface().commit();
    expect_lock_surface_error(
        &mut f,
        id,
        ext_session_lock_surface_v1::Error::CommitBeforeFirstAck,
        "the synthetic ack that answered the orphaned commit must not survive \
         into the next role, where it would mask a genuine commit-before-ack",
    );
}

/// "driftwm configured this" is a verdict on the *role*, and a `wl_surface`
/// outlives its roles — so the two can disagree. A lock screen that loses the
/// session between one lock and the next brings its old surfaces to a lock
/// driftwm refuses: the surface's previous role was configured, the role it
/// takes now is declined and stays live and unconfigured forever, and its
/// commits are real violations the client has to be told about. That is why the
/// verdict is cleared as the new role is taken, above the early return that
/// declines it — a reset placed after the return would never run on exactly the
/// role that needs it.
#[test]
fn a_declined_lock_role_inherits_no_configured_verdict_from_the_role_before_it() {
    let mut f = Fixture::new();
    // The incumbent's lock is still up at teardown, so `lock_surfaces` never
    // drains.
    f.skip_baseline_check();
    let output = f.add_output(1, (1920, 1080));
    let refused = f.add_client();
    let incumbent = f.add_client();

    // The refused client's first lock is its own, and its role is configured.
    orphan_the_role(&mut f, refused, &output);
    let surface = f.client(refused).last_lock_surface().surface.clone();

    // Someone else takes the session while it is down.
    f.client(incumbent).lock_session();
    f.roundtrip(incumbent);
    confirm_lock(&mut f, incumbent, &output);

    // Refused, but it puts a lock surface on its refused lock anyway — on the
    // same `wl_surface` as before.
    f.client(refused).lock_session();
    f.roundtrip(refused);
    let wl_output = f.client(refused).output(&output.name());
    f.client(refused).retake_lock_surface(&surface, &wl_output);
    f.double_roundtrip(refused);
    assert!(
        f.client(refused)
            .last_lock_surface()
            .configures_received
            .is_empty(),
        "precondition: driftwm declined to configure the refused client's role"
    );

    f.client(refused).last_lock_surface().commit();
    expect_lock_surface_error(
        &mut f,
        refused,
        ext_session_lock_surface_v1::Error::CommitBeforeFirstAck,
        "a declined role must get smithay's real error even on a `wl_surface` \
         whose previous role driftwm did configure",
    );
}
