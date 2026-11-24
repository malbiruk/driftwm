use super::input_backend::{key_press, key_release};
use super::{Fixture, settle};
use driftwm::config::Config;

fn fixture(modifier: &str, mode: &str) -> Fixture {
    let config = Config::from_toml(&format!(r#"[held-keybindings]
"{modifier}+F8" = {{ press = "zoom-out", release = "zoom-reset", cancel = "zoom-in", hold = "{mode}" }}
"#)).unwrap();
    let mut f = Fixture::with_config(config);
    f.add_output(1, (1920, 1080));
    f.skip_baseline_check();
    f
}

#[test]
fn arbitrary_modifier_hold_ignores_trigger_release_and_repeated_press() {
    for (modifier, key) in [("alt", 56), ("ctrl", 29), ("shift", 42), ("super", 125)] {
        let mut f = fixture(modifier, "modifiers");
        key_press(&mut f, key);
        key_press(&mut f, 66);
        key_release(&mut f, 66);
        settle(&mut f);
        let zoom = f.state().zoom();
        assert!(zoom < 1.0);
        key_press(&mut f, 66);
        key_release(&mut f, 66);
        settle(&mut f);
        assert_eq!(f.state().zoom(), zoom);
        assert!(f.state().has_active_held_bindings());
        key_release(&mut f, key);
        settle(&mut f);
        assert!((f.state().zoom() - 1.0).abs() < 1e-6);
        assert!(f.state().held_bindings.is_empty());
    }
}

#[test]
fn key_hold_waits_for_trigger_even_when_modifier_lifts_first() {
    let mut f = fixture("alt", "key");
    key_press(&mut f, 56);
    key_press(&mut f, 66);
    key_release(&mut f, 56);
    settle(&mut f);
    assert!(f.state().zoom() < 1.0);
    key_release(&mut f, 66);
    settle(&mut f);
    assert!((f.state().zoom() - 1.0).abs() < 1e-6);
}

#[test]
fn escape_cancels_once_and_suppresses_late_release() {
    let mut f = fixture("alt", "modifiers");
    key_press(&mut f, 56);
    key_press(&mut f, 66);
    key_release(&mut f, 66);
    settle(&mut f);
    key_press(&mut f, 1);
    key_release(&mut f, 1);
    settle(&mut f);
    assert!(!f.state().has_active_held_bindings());
    assert!((f.state().zoom() - 1.0).abs() < 1e-6);
    key_press(&mut f, 66);
    key_release(&mut f, 66);
    settle(&mut f);
    assert!((f.state().zoom() - 1.0).abs() < 1e-6);
    // A distinguishable zoom proves release was suppressed, not executed again.
    f.state().with_output_state(|os| os.zoom = 0.5);
    key_release(&mut f, 56);
    settle(&mut f);
    assert!((f.state().zoom() - 0.5).abs() < 1e-6);
}

#[test]
fn input_reset_discards_pending_release_and_cancel_actions() {
    let mut f = fixture("alt", "modifiers");
    key_press(&mut f, 56);
    key_press(&mut f, 66);
    settle(&mut f);
    let zoom = f.state().zoom();
    f.state().reset_held_input_state();
    key_release(&mut f, 66);
    key_release(&mut f, 56);
    settle(&mut f);
    assert_eq!(f.state().zoom(), zoom);
    assert!(f.state().held_bindings.is_empty());
}
