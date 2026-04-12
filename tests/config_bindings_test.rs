use driftwm::config::{Action, Config, Direction};
use smithay::input::keyboard::{Keysym, ModifiersState, keysyms};

// ── Modifier helpers ─────────────────────────────────────────────────────

fn mods(alt: bool, ctrl: bool, shift: bool, logo: bool) -> ModifiersState {
    ModifiersState {
        alt,
        ctrl,
        shift,
        logo,
        ..ModifiersState::default()
    }
}

fn logo() -> ModifiersState {
    mods(false, false, false, true)
}

fn logo_shift() -> ModifiersState {
    mods(false, false, true, true)
}

fn logo_ctrl() -> ModifiersState {
    mods(false, true, false, true)
}

fn alt() -> ModifiersState {
    mods(true, false, false, false)
}

fn alt_shift() -> ModifiersState {
    mods(true, false, true, false)
}

fn no_mods() -> ModifiersState {
    ModifiersState::default()
}

// ── Super+Return → Exec ──────────────────────────────────────────────────

#[test]
fn super_return_resolves_to_exec() {
    let config = Config::default();
    let result = config.lookup(&logo(), Keysym::from(keysyms::KEY_Return));
    assert!(result.is_some(), "Super+Return should be bound");
    assert!(
        matches!(result.unwrap(), Action::Exec(_)),
        "Super+Return should resolve to Exec"
    );
}

// ── Super+q → CloseWindow ────────────────────────────────────────────────

#[test]
fn super_q_resolves_to_close_window() {
    let config = Config::default();
    let result = config.lookup(&logo(), Keysym::from(keysyms::KEY_q));
    assert!(result.is_some(), "Super+q should be bound");
    assert!(
        matches!(result.unwrap(), Action::CloseWindow),
        "Super+q should resolve to CloseWindow"
    );
}

// ── Super+Shift+Arrow → NudgeWindow ─────────────────────────────────────

#[test]
fn super_shift_up_resolves_to_nudge_window_up() {
    let config = Config::default();
    let result = config.lookup(&logo_shift(), Keysym::from(keysyms::KEY_Up));
    assert!(result.is_some(), "Super+Shift+Up should be bound");
    assert!(
        matches!(result.unwrap(), Action::NudgeWindow(Direction::Up)),
        "Super+Shift+Up should resolve to NudgeWindow(Up)"
    );
}

#[test]
fn super_shift_down_resolves_to_nudge_window_down() {
    let config = Config::default();
    let result = config.lookup(&logo_shift(), Keysym::from(keysyms::KEY_Down));
    assert!(
        matches!(result.unwrap(), Action::NudgeWindow(Direction::Down)),
        "Super+Shift+Down should resolve to NudgeWindow(Down)"
    );
}

#[test]
fn super_shift_left_resolves_to_nudge_window_left() {
    let config = Config::default();
    let result = config.lookup(&logo_shift(), Keysym::from(keysyms::KEY_Left));
    assert!(
        matches!(result.unwrap(), Action::NudgeWindow(Direction::Left)),
        "Super+Shift+Left should resolve to NudgeWindow(Left)"
    );
}

#[test]
fn super_shift_right_resolves_to_nudge_window_right() {
    let config = Config::default();
    let result = config.lookup(&logo_shift(), Keysym::from(keysyms::KEY_Right));
    assert!(
        matches!(result.unwrap(), Action::NudgeWindow(Direction::Right)),
        "Super+Shift+Right should resolve to NudgeWindow(Right)"
    );
}

// ── Super+Ctrl+Arrow → PanViewport ──────────────────────────────────────

#[test]
fn super_ctrl_left_resolves_to_pan_viewport_left() {
    let config = Config::default();
    let result = config.lookup(&logo_ctrl(), Keysym::from(keysyms::KEY_Left));
    assert!(result.is_some(), "Super+Ctrl+Left should be bound");
    assert!(
        matches!(result.unwrap(), Action::PanViewport(Direction::Left)),
        "Super+Ctrl+Left should resolve to PanViewport(Left)"
    );
}

#[test]
fn super_ctrl_right_resolves_to_pan_viewport_right() {
    let config = Config::default();
    let result = config.lookup(&logo_ctrl(), Keysym::from(keysyms::KEY_Right));
    assert!(
        matches!(result.unwrap(), Action::PanViewport(Direction::Right)),
        "Super+Ctrl+Right should resolve to PanViewport(Right)"
    );
}

#[test]
fn super_ctrl_up_resolves_to_pan_viewport_up() {
    let config = Config::default();
    let result = config.lookup(&logo_ctrl(), Keysym::from(keysyms::KEY_Up));
    assert!(
        matches!(result.unwrap(), Action::PanViewport(Direction::Up)),
        "Super+Ctrl+Up should resolve to PanViewport(Up)"
    );
}

#[test]
fn super_ctrl_down_resolves_to_pan_viewport_down() {
    let config = Config::default();
    let result = config.lookup(&logo_ctrl(), Keysym::from(keysyms::KEY_Down));
    assert!(
        matches!(result.unwrap(), Action::PanViewport(Direction::Down)),
        "Super+Ctrl+Down should resolve to PanViewport(Down)"
    );
}

// ── Unbound / wrong modifier → None ─────────────────────────────────────

#[test]
fn unbound_key_returns_none() {
    let config = Config::default();
    let result = config.lookup(&no_mods(), Keysym::from(keysyms::KEY_a));
    assert!(
        result.is_none(),
        "bare 'a' with no modifiers should not be bound"
    );
}

#[test]
fn ctrl_return_returns_none_when_only_super_return_is_bound() {
    let config = Config::default();
    let ctrl_only = mods(false, true, false, false);
    let result = config.lookup(&ctrl_only, Keysym::from(keysyms::KEY_Return));
    assert!(
        result.is_none(),
        "Ctrl+Return should not be bound (only Super+Return is)"
    );
}

#[test]
fn bare_return_returns_none() {
    let config = Config::default();
    let result = config.lookup(&no_mods(), Keysym::from(keysyms::KEY_Return));
    assert!(
        result.is_none(),
        "Return without modifiers should not be bound"
    );
}

#[test]
fn super_shift_return_returns_none() {
    let config = Config::default();
    let result = config.lookup(&logo_shift(), Keysym::from(keysyms::KEY_Return));
    assert!(result.is_none(), "Super+Shift+Return should not be bound");
}

// ── Super+a → HomeToggle ─────────────────────────────────────────────────

#[test]
fn super_a_resolves_to_home_toggle() {
    let config = Config::default();
    let result = config.lookup(&logo(), Keysym::from(keysyms::KEY_a));
    assert!(result.is_some(), "Super+a should be bound");
    assert!(
        matches!(result.unwrap(), Action::HomeToggle),
        "Super+a should resolve to HomeToggle"
    );
}

// ── Super+c → CenterWindow ───────────────────────────────────────────────

#[test]
fn super_c_resolves_to_center_window() {
    let config = Config::default();
    let result = config.lookup(&logo(), Keysym::from(keysyms::KEY_c));
    assert!(result.is_some(), "Super+c should be bound");
    assert!(
        matches!(result.unwrap(), Action::CenterWindow),
        "Super+c should resolve to CenterWindow"
    );
}

#[test]
fn super_c_does_not_conflict_with_close_window() {
    let config = Config::default();
    let result = config.lookup(&logo(), Keysym::from(keysyms::KEY_c));
    assert!(
        !matches!(result, Some(Action::CloseWindow)),
        "Super+c must not resolve to CloseWindow (that is Super+q)"
    );
}

// ── Super+Arrow → CenterNearest ─────────────────────────────────────────

#[test]
fn super_up_resolves_to_center_nearest_up() {
    let config = Config::default();
    let result = config.lookup(&logo(), Keysym::from(keysyms::KEY_Up));
    assert!(result.is_some(), "Super+Up should be bound");
    assert!(
        matches!(result.unwrap(), Action::CenterNearest(Direction::Up)),
        "Super+Up should resolve to CenterNearest(Up)"
    );
}

#[test]
fn super_down_resolves_to_center_nearest_down() {
    let config = Config::default();
    let result = config.lookup(&logo(), Keysym::from(keysyms::KEY_Down));
    assert!(result.is_some(), "Super+Down should be bound");
    assert!(
        matches!(result.unwrap(), Action::CenterNearest(Direction::Down)),
        "Super+Down should resolve to CenterNearest(Down)"
    );
}

#[test]
fn super_left_resolves_to_center_nearest_left() {
    let config = Config::default();
    let result = config.lookup(&logo(), Keysym::from(keysyms::KEY_Left));
    assert!(result.is_some(), "Super+Left should be bound");
    assert!(
        matches!(result.unwrap(), Action::CenterNearest(Direction::Left)),
        "Super+Left should resolve to CenterNearest(Left)"
    );
}

#[test]
fn super_right_resolves_to_center_nearest_right() {
    let config = Config::default();
    let result = config.lookup(&logo(), Keysym::from(keysyms::KEY_Right));
    assert!(result.is_some(), "Super+Right should be bound");
    assert!(
        matches!(result.unwrap(), Action::CenterNearest(Direction::Right)),
        "Super+Right should resolve to CenterNearest(Right)"
    );
}

// ── Alt+Tab → CycleWindows (default cycle modifier is Alt) ───────────────

#[test]
fn alt_tab_resolves_to_cycle_windows_forward() {
    let config = Config::default();
    let result = config.lookup(&alt(), Keysym::from(keysyms::KEY_Tab));
    assert!(result.is_some(), "Alt+Tab should be bound");
    assert!(
        matches!(result.unwrap(), Action::CycleWindows { backward: false }),
        "Alt+Tab should resolve to CycleWindows {{ backward: false }}"
    );
}

#[test]
fn alt_shift_tab_resolves_to_cycle_windows_backward() {
    let config = Config::default();
    let result = config.lookup(&alt_shift(), Keysym::from(keysyms::KEY_ISO_Left_Tab));
    assert!(result.is_some(), "Alt+Shift+Tab should be bound");
    assert!(
        matches!(result.unwrap(), Action::CycleWindows { backward: true }),
        "Alt+Shift+Tab should resolve to CycleWindows {{ backward: true }}"
    );
}
