//! Per-touchscreen output routing (`[[input.touch.devices]]`). A touchscreen
//! is a display, so its touches must land on the output it overlays — and two
//! touchscreens on two outputs must route independently, which the global
//! `[input.touch] map_to_output` cannot express. The fake touchscreen's device
//! name is what the entries match against; the real libinput tag and
//! physical-size heuristics need no coverage here because a matched entry
//! claims the device before any of them run.

use smithay::utils::{Logical, Point};

use super::input_backend::touch_down;
use super::{Fixture, config};

#[test]
fn a_device_entry_pins_touch_to_its_output() {
    let mut f = Fixture::with_config(config(
        r#"
            [[input.touch.devices]]
            name = "fake-touchscreen"
            map_to_output = "HEADLESS-2"
            "#,
    ));
    let out1 = f.add_output(1, (1920, 1080));
    let out2 = f.add_output(2, (1920, 1080));
    assert_eq!(
        f.state().focused_output.as_ref(),
        Some(&out1),
        "the first output must start active, or this scenario tests nothing"
    );

    touch_down(&mut f, Point::<f64, Logical>::from((0.0, 0.0)), 0);

    // A device pinned to one output makes that output active, or every
    // camera and layer read afterwards acts on the wrong monitor.
    assert_eq!(
        f.state().focused_output.as_ref(),
        Some(&out2),
        "a touchscreen with a matching entry must touch the entry's output"
    );
}

/// An entry with `map_to_output = "none"` opts its device out of the global
/// pin: the matched device runs the auto-detect chain (here: first output),
/// while an unmatched device would still get the global. This is what lets a
/// multi-touchscreen setup keep auto-detection for one panel and a manual
/// route for the other.
#[test]
fn a_matched_device_with_none_ignores_the_global_pin() {
    let mut f = Fixture::with_config(config(
        r#"
            [input.touch]
            map_to_output = "HEADLESS-2"

            [[input.touch.devices]]
            name = "fake-touchscreen"
            map_to_output = "none"
            "#,
    ));
    let out1 = f.add_output(1, (1920, 1080));
    let out2 = f.add_output(2, (1920, 1080));

    touch_down(&mut f, Point::<f64, Logical>::from((0.0, 0.0)), 0);

    assert_eq!(
        f.state().focused_output.as_ref(),
        Some(&out1),
        "a matched entry with \"none\" must skip the global pin, not land on it"
    );
    assert_ne!(*out1.name(), *out2.name(), "or this scenario tests nothing");
}

#[test]
fn an_unmatched_device_still_gets_the_global_pin() {
    let mut f = Fixture::with_config(config(
        r#"
            [input.touch]
            map_to_output = "HEADLESS-2"

            [[input.touch.devices]]
            name = "some other panel"
            map_to_output = "HEADLESS-2"
            "#,
    ));
    let out1 = f.add_output(1, (1920, 1080));
    let out2 = f.add_output(2, (1920, 1080));

    touch_down(&mut f, Point::<f64, Logical>::from((0.0, 0.0)), 0);

    assert_eq!(
        f.state().focused_output.as_ref(),
        Some(&out2),
        "a touchscreen no entry matches must fall through to the global pin"
    );
    assert_ne!(*out1.name(), *out2.name(), "or this scenario tests nothing");
}
