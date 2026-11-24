//! The IPC `bindings` verb: the effective key, tap and mouse bindings, in
//! config syntax, after defaults and the user's config are merged.

use super::{Fixture, config};
use crate::ipc::dispatch;
use crate::ipc::protocol::{BindingInfo, BindingsInfo, Request, Response};

fn bindings(f: &mut Fixture) -> BindingsInfo {
    match dispatch(Request::Bindings, f.state()) {
        Ok(Response::Bindings(info)) => info,
        other => panic!("expected a Bindings reply, got {other:?}"),
    }
}

fn action_of<'a>(list: &'a [BindingInfo], trigger: &str) -> Option<&'a str> {
    list.iter()
        .find(|b| b.trigger == trigger)
        .map(|b| b.action.as_str())
}

#[test]
fn lists_the_defaults_in_config_syntax() {
    let mut f = Fixture::new();
    let info = bindings(&mut f);

    assert_eq!(info.mod_key, "super");
    assert_eq!(action_of(&info.keys, "super+Return"), Some("exec-terminal"));
    assert_eq!(action_of(&info.keys, "super+Up"), Some("focus-nearest up"));
    assert_eq!(
        action_of(&info.keys, "alt+Tab"),
        Some("cycle-windows forward")
    );
}

#[test]
fn user_bindings_override_defaults_and_none_unbinds() {
    let mut f = Fixture::with_config(config(
        r#"
[keybindings]
"mod+return" = "exec foot"
"mod+q" = "none"
"mod+w" = "spawn notify-send hi"
"#,
    ));
    let info = bindings(&mut f);

    assert_eq!(action_of(&info.keys, "super+Return"), Some("exec foot"));
    assert_eq!(action_of(&info.keys, "super+q"), None);
    assert_eq!(
        action_of(&info.keys, "super+w"),
        Some("spawn notify-send hi")
    );
}

#[test]
fn disabled_default_categories_are_absent() {
    let mut f = Fixture::with_config(config("[bindings]\ndisable_defaults = [\"keys\"]\n"));
    assert!(bindings(&mut f).keys.is_empty());
}

#[test]
fn tap_chords_are_listed_apart_from_keys() {
    let mut f = Fixture::with_config(config(
        "[keybindings]\n\"alt+shift\" = \"switch-layout next\"\n",
    ));
    let info = bindings(&mut f);

    assert_eq!(
        action_of(&info.taps, "alt+shift"),
        Some("switch-layout next")
    );
    assert!(info.keys.iter().all(|b| b.trigger != "alt+shift"));
}

#[test]
fn mouse_bindings_carry_their_context() {
    let mut f = Fixture::new();
    let info = bindings(&mut f);

    assert!(!info.mouse.is_empty());
    assert!(
        info.mouse
            .iter()
            .any(|b| b.trigger == "alt+left" && b.action == "move-window"),
        "no alt+left move-window default in {:?}",
        info.mouse
    );
}

#[test]
fn every_list_is_sorted_by_trigger() {
    let mut f = Fixture::new();
    let info = bindings(&mut f);

    for list in [&info.keys, &info.taps, &info.mouse] {
        let triggers: Vec<_> = list.iter().map(|b| (&b.trigger, &b.context)).collect();
        assert!(triggers.is_sorted());
    }
}

#[test]
fn every_listed_binding_parses_back_to_the_same_action() {
    use driftwm::config::{ModKey, parse_action, parse_key_combo};

    let mut f = Fixture::new();
    for b in bindings(&mut f).keys {
        let combo = parse_key_combo(&b.trigger, ModKey::Super)
            .unwrap_or_else(|e| panic!("trigger {:?} does not parse: {e}", b.trigger));
        assert_eq!(combo.to_string(), b.trigger);
        let action = parse_action(&b.action)
            .unwrap_or_else(|e| panic!("action {:?} does not parse: {e}", b.action));
        assert_eq!(action.to_string(), b.action);
    }
}
