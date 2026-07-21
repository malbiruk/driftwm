//! Alt-Tab cycle session lifecycle. A modifier-less cycle source (gesture, bare
//! wheel) has no key release to end the session, so each fire commits its step
//! immediately (single step, fresh anchor). A held-modifier source (alt+tab,
//! mod+wheel) keeps the session — the keyboard check / focus choke ends it on
//! release or the next non-cycle focus change / action.
//!
//! Fixture limitation: the in-process harness has no synthetic keyboard input,
//! so a real held modifier can't be driven. `held_cycle_step` reproduces the
//! held-modifier arm (step + cycle-navigating navigate, no commit) to stand in.

use driftwm::config::Action;
use smithay::desktop::Window;
use smithay::utils::SERIAL_COUNTER;

use super::{Fixture, client::ClientId, map_window, window_by_app_id};

/// Map toplevels a, b, c on a fresh client; focus history ends [c, b, a].
/// Returns the client so callers can map more windows.
fn three_windows(f: &mut Fixture) -> ClientId {
    let id = f.add_client();
    map_window(f, id, "a", (400, 300));
    map_window(f, id, "b", (400, 300));
    map_window(f, id, "c", (400, 300));
    id
}

/// A modifier-less cycle fire: `execute_action` steps then, seeing no cycle-hold
/// modifier held, commits immediately.
fn cycle_forward(f: &mut Fixture) {
    f.state()
        .execute_action(&Action::CycleWindows { backward: false });
}

/// One step of a held-modifier cycle: step + a cycle-navigating navigate, with no
/// commit — exactly the `CycleWindows` arm minus the modifier-gated `end_cycle`.
/// Stands in for a real held modifier the fixture can't inject.
fn held_cycle_step(f: &mut Fixture) {
    let anchor = f.state().cycle_anchor();
    if let Some(target) = f.state().stage.cycle_step(false, anchor.as_ref()) {
        let window = target.client().expect("cycle target is a client").clone();
        f.state().cycle_navigating = true;
        f.state().navigate_to_window(&window, false);
        f.state().cycle_navigating = false;
    }
}

/// Two held-modifier steps, returning the selected window (focus_history[state]).
fn open_two_step_session(f: &mut Fixture) -> Window {
    held_cycle_step(f);
    held_cycle_step(f);
    let idx = f.state().stage.cycle_state().expect("mid-cycle");
    f.state().stage.focus_history()[idx]
        .client()
        .expect("cycle selection is a client")
        .clone()
}

#[test]
fn modifier_less_fires_commit_each_step() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    three_windows(&mut f);
    let win_b = window_by_app_id(&mut f, "b").unwrap();
    let win_c = window_by_app_id(&mut f, "c").unwrap();

    // No modifier held: each fire is a single step that commits immediately, so
    // the two most-recent windows toggle and no session lingers.
    cycle_forward(&mut f);
    assert_eq!(f.state().stage.cycle_state(), None, "no lingering session");
    assert_eq!(
        f.state()
            .stage
            .focus_history()
            .first()
            .and_then(|w| w.client()),
        Some(&win_b),
        "first fire commits the previous window"
    );

    cycle_forward(&mut f);
    assert_eq!(f.state().stage.cycle_state(), None, "still no session");
    assert_eq!(
        f.state()
            .stage
            .focus_history()
            .first()
            .and_then(|w| w.client()),
        Some(&win_c),
        "second fire toggles back"
    );
}

#[test]
fn held_session_digs_deeper_without_committing() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    three_windows(&mut f);

    let before = f.state().stage.focus_history().to_vec();
    held_cycle_step(&mut f);
    let after_one = f.state().stage.cycle_state();
    held_cycle_step(&mut f);
    let after_two = f.state().stage.cycle_state();

    // While the modifier is held the session persists and digs deeper without
    // reordering the frozen history.
    assert!(after_one.is_some(), "first step opens a session");
    assert!(after_two.is_some(), "second step keeps it open");
    assert_ne!(after_one, after_two, "the second step digs deeper");
    assert_eq!(
        f.state().stage.focus_history().to_vec(),
        before,
        "held cycling must not reorder history until it ends"
    );
}

#[test]
fn non_cycle_action_commits_the_selection() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    three_windows(&mut f);
    let selected = open_two_step_session(&mut f);

    // A non-focus-changing action ends the held session via the execute_action choke.
    f.state().execute_action(&Action::ZoomReset);

    assert_eq!(f.state().stage.cycle_state(), None, "the session ended");
    assert_eq!(
        f.state()
            .stage
            .focus_history()
            .first()
            .and_then(|w| w.client()),
        Some(&selected),
        "the selected window is committed to the MRU head"
    );
}

#[test]
fn click_focus_during_cycle_commits_then_promotes() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    three_windows(&mut f);
    let selected = open_two_step_session(&mut f);

    // A real focus change through the click-to-focus path: raise_and_focus runs
    // update_keyboard_focus → focus_changed, hitting the cycle choke. Focus a
    // window other than the selection.
    let clicked = window_by_app_id(&mut f, "c").unwrap();
    assert_ne!(
        clicked, selected,
        "focus a different window than the selection"
    );
    let serial = SERIAL_COUNTER.next_serial();
    f.state().raise_and_focus(&clicked, serial);

    assert_eq!(f.state().stage.cycle_state(), None, "the session ended");
    let history = f.state().stage.focus_history();
    assert_eq!(
        history.first().and_then(|w| w.client()),
        Some(&clicked),
        "clicked window on top"
    );
    assert_eq!(
        history.get(1).and_then(|w| w.client()),
        Some(&selected),
        "the selection committed just under the clicked window, not lost"
    );
}

#[test]
fn mapping_a_window_during_cycle_commits_the_selection() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = three_windows(&mut f);
    let selected = open_two_step_session(&mut f);

    // A newly mapped window takes focus at map time → focus_changed → the cycle
    // choke commits the selection before the new window promotes.
    map_window(&mut f, id, "d", (400, 300));
    let mapped = window_by_app_id(&mut f, "d").unwrap();

    assert_eq!(f.state().stage.cycle_state(), None, "the session ended");
    let history = f.state().stage.focus_history();
    assert_eq!(
        history.first().and_then(|w| w.client()),
        Some(&mapped),
        "the new window is on top"
    );
    assert_eq!(
        history.get(1).and_then(|w| w.client()),
        Some(&selected),
        "the selection committed just under the new window, not lost"
    );
}
