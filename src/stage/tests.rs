use smithay::utils::{Point, Size};

use super::mock::TestWindow;
use super::{ElementId, PinnedSite, Stage, StageElement, subtree_raise_order};

fn stage_with(n: u64) -> (Stage<TestWindow>, Vec<TestWindow>) {
    let mut stage = Stage::new();
    let windows: Vec<TestWindow> = (0..n).map(TestWindow::new).collect();
    for (i, w) in windows.iter().enumerate() {
        stage.map(w.clone(), Point::from((i as i32 * 10, 0)));
    }
    (stage, windows)
}

fn z_labels(stage: &Stage<TestWindow>) -> Vec<u64> {
    stage.windows().map(|w| w.label()).collect()
}

/// Step the cycle with focus where the history says it is (the head) — the
/// ordinary case, as opposed to focus parked on a window the history skips.
fn cycle_step(stage: &mut Stage<TestWindow>, backward: bool) -> Option<TestWindow> {
    let focused = stage.focus_history().first().cloned();
    stage.cycle_step(backward, focused.as_ref())
}

#[test]
fn map_inserts_on_top_and_remap_raises() {
    let (mut stage, windows) = stage_with(3);
    assert_eq!(z_labels(&stage), vec![0, 1, 2]);

    stage.map(windows[0].clone(), Point::from((50, 50)));
    assert_eq!(z_labels(&stage), vec![1, 2, 0]);
    assert_eq!(stage.position_of(&windows[0]), Some(Point::from((50, 50))));
    stage.verify_invariants();
}

#[test]
fn map_assigns_stable_unique_ids() {
    let (mut stage, windows) = stage_with(2);
    let id0 = stage.id_of(&windows[0]).unwrap();
    let id1 = stage.id_of(&windows[1]).unwrap();
    assert_ne!(id0, id1);

    // Remapping keeps the id; a fresh window gets a new one.
    stage.map(windows[0].clone(), Point::from((5, 5)));
    assert_eq!(stage.id_of(&windows[0]), Some(id0));
    let w = TestWindow::new(99);
    stage.map(w.clone(), Point::from((0, 0)));
    assert!(stage.id_of(&w).unwrap() > id1);
}

#[test]
fn window_by_id_looks_up_by_stable_id() {
    let (mut stage, windows) = stage_with(2);
    let id0 = stage.id_of(&windows[0]).unwrap();
    let id1 = stage.id_of(&windows[1]).unwrap();
    assert_ne!(id0, id1);
    assert_eq!(stage.window_by_id(id0), Some(&windows[0]));
    assert_eq!(stage.window_by_id(id1), Some(&windows[1]));
    assert_eq!(stage.window_by_id(ElementId(999)), None);

    // Ids survive a raise and a re-map (position change).
    stage.raise(&windows[0]);
    stage.map(windows[1].clone(), Point::from((7, 7)));
    assert_eq!(stage.window_by_id(id0), Some(&windows[0]));
    assert_eq!(stage.window_by_id(id1), Some(&windows[1]));

    // A remapped window (removed, then mapped again) gets a fresh id, and the
    // old id no longer resolves.
    stage.remove(&windows[0]);
    stage.map(windows[0].clone(), Point::from((0, 0)));
    let new_id = stage.id_of(&windows[0]).unwrap();
    assert_ne!(new_id, id0);
    assert_eq!(stage.window_by_id(id0), None);
    assert_eq!(stage.window_by_id(new_id), Some(&windows[0]));
}

#[test]
fn raise_moves_to_top_and_ignores_unknown() {
    let (mut stage, windows) = stage_with(3);
    stage.raise(&windows[1]);
    assert_eq!(z_labels(&stage), vec![0, 2, 1]);

    let unknown = TestWindow::new(42);
    stage.raise(&unknown);
    assert_eq!(z_labels(&stage), vec![0, 2, 1]);
}

#[test]
fn remove_purges_everywhere_and_clamps_cycle() {
    let (mut stage, windows) = stage_with(3);
    for w in &windows {
        stage.push_focus(w);
    }
    // History (MRU): [2, 1, 0]. Point the cycle at the last entry.
    cycle_step(&mut stage, false);
    cycle_step(&mut stage, false);
    assert_eq!(stage.cycle_state(), Some(2));

    stage.set_fullscreen(
        "DP-1",
        windows[0].clone(),
        Point::from((0, 0)),
        Size::from((100, 100)),
    );
    stage.remove(&windows[0]);

    assert!(!stage.contains(&windows[0]));
    assert!(!stage.focus_history().contains(&windows[0]));
    assert!(stage.fullscreen_on("DP-1").is_none());
    // Cycle index clamped into the shrunk history.
    assert_eq!(stage.cycle_state(), Some(1));
    stage.verify_invariants();
}

#[test]
fn remove_last_history_entry_cancels_cycle() {
    let (mut stage, windows) = stage_with(1);
    stage.push_focus(&windows[0]);
    cycle_step(&mut stage, false);
    assert_eq!(stage.cycle_state(), Some(0));

    stage.remove(&windows[0]);
    assert_eq!(stage.cycle_state(), None);
    stage.verify_invariants();
}

#[test]
fn push_focus_moves_to_front_without_duplicates() {
    let (mut stage, windows) = stage_with(3);
    stage.push_focus(&windows[0]);
    stage.push_focus(&windows[1]);
    stage.push_focus(&windows[0]);
    let labels: Vec<u64> = stage.focus_history().iter().map(|w| w.label()).collect();
    assert_eq!(labels, vec![0, 1]);
    stage.verify_invariants();
}

#[test]
fn cycle_first_step_goes_to_previous_window() {
    let (mut stage, windows) = stage_with(3);
    for w in &windows {
        stage.push_focus(w);
    }
    // History: [2, 1, 0]; first Tab jumps to index 1.
    let target = cycle_step(&mut stage, false).unwrap();
    assert_eq!(target.label(), 1);
    assert_eq!(stage.cycle_state(), Some(1));

    let target = cycle_step(&mut stage, false).unwrap();
    assert_eq!(target.label(), 0);

    // Wraps around.
    let target = cycle_step(&mut stage, false).unwrap();
    assert_eq!(target.label(), 2);
}

#[test]
fn cycle_from_a_window_outside_the_history_starts_at_its_head() {
    let (mut stage, windows) = stage_with(3);
    stage.push_focus(&windows[0]);
    stage.push_focus(&windows[1]);
    // History: [1, 0]. Focus sits on a pinned window, which never enters it, so
    // the head is the window to go back to — not one to step over.
    stage.set_pin(
        &windows[2],
        PinnedSite {
            output: "DP-1".to_string(),
            screen_pos: Point::from((0, 0)),
        },
    );
    let target = stage.cycle_step(false, Some(&windows[2])).unwrap();
    assert_eq!(target.label(), 1);
    assert_eq!(stage.cycle_state(), Some(0));
}

#[test]
fn cycle_from_a_window_at_the_history_head_steps_past_it() {
    let (mut stage, windows) = stage_with(3);
    for w in &windows {
        stage.push_focus(w);
    }
    // History: [2, 1, 0] with focus on the head — a modal dialog over window 2
    // resolves to window 2 (`cycle_anchor`), so Tab must still step past it.
    let target = stage.cycle_step(false, Some(&windows[2])).unwrap();
    assert_eq!(target.label(), 1);
}

#[test]
fn cycle_backward_wraps_to_oldest() {
    let (mut stage, windows) = stage_with(3);
    for w in &windows {
        stage.push_focus(w);
    }
    // History: [2, 1, 0]; the first step with no cycle state is always
    // index 1, regardless of direction.
    let target = cycle_step(&mut stage, true).unwrap();
    assert_eq!(target.label(), 1);
    let target = cycle_step(&mut stage, true).unwrap();
    assert_eq!(target.label(), 2);
}

#[test]
fn cycle_skips_fullscreen_windows() {
    let (mut stage, windows) = stage_with(3);
    for w in &windows {
        stage.push_focus(w);
    }
    // History: [2, 1, 0]. Fullscreen the would-be first target (index 1).
    stage.set_fullscreen(
        "DP-1",
        windows[1].clone(),
        Point::from((0, 0)),
        Size::from((100, 100)),
    );
    let target = cycle_step(&mut stage, false).unwrap();
    assert_eq!(target.label(), 0);
}

#[test]
fn cycle_all_fullscreen_yields_none() {
    let (mut stage, windows) = stage_with(2);
    for w in &windows {
        stage.push_focus(w);
    }
    stage.set_fullscreen(
        "DP-1",
        windows[0].clone(),
        Point::from((0, 0)),
        Size::from((100, 100)),
    );
    stage.set_fullscreen(
        "DP-2",
        windows[1].clone(),
        Point::from((0, 0)),
        Size::from((100, 100)),
    );
    assert!(cycle_step(&mut stage, false).is_none());
    assert_eq!(stage.cycle_state(), None);
}

#[test]
fn end_cycle_commits_selection_to_front() {
    let (mut stage, windows) = stage_with(3);
    for w in &windows {
        stage.push_focus(w);
    }
    cycle_step(&mut stage, false);
    cycle_step(&mut stage, false); // now pointing at label 0 (index 2)
    stage.end_cycle();
    assert_eq!(stage.cycle_state(), None);
    let labels: Vec<u64> = stage.focus_history().iter().map(|w| w.label()).collect();
    assert_eq!(labels, vec![0, 2, 1]);
}

#[test]
fn end_cycle_without_active_cycle_is_noop() {
    let (mut stage, windows) = stage_with(2);
    for w in &windows {
        stage.push_focus(w);
    }
    stage.end_cycle();
    let labels: Vec<u64> = stage.focus_history().iter().map(|w| w.label()).collect();
    assert_eq!(labels, vec![1, 0]);
}

#[test]
fn drop_from_focus_history_clamps_cycle_index() {
    let (mut stage, windows) = stage_with(3);
    for w in &windows {
        stage.push_focus(w);
    }
    cycle_step(&mut stage, false);
    // Pin-style removal: an in-bounds index survives...
    stage.drop_from_focus_history(&windows[2]);
    assert_eq!(stage.cycle_state(), Some(1));
    // ...and one pushed out of bounds clamps instead of going stale.
    stage.drop_from_focus_history(&windows[0]);
    assert_eq!(stage.cycle_state(), Some(0));
    stage.verify_invariants();
}

#[test]
fn raise_with_children_stacks_child_directly_above_parent() {
    let (mut stage, windows) = stage_with(4);
    windows[1].set_parent(Some(windows[0].clone()));
    windows[2].set_parent(Some(windows[1].clone()));

    // Raise unrelated 3 on top first, then raise 0's subtree.
    stage.raise(&windows[3]);
    stage.raise_with_children(&windows[0]);
    assert_eq!(z_labels(&stage), vec![3, 0, 1, 2]);
    stage.verify_invariants();
}

#[test]
fn raise_with_children_leaves_unrelated_windows_alone() {
    let (mut stage, windows) = stage_with(3);
    windows[2].set_parent(Some(windows[0].clone()));
    stage.raise_with_children(&windows[0]);
    // 1 keeps its slot below the raised subtree.
    assert_eq!(z_labels(&stage), vec![1, 0, 2]);
}

#[test]
fn enforce_stacking_pushes_widgets_down_and_fullscreen_up() {
    let (mut stage, windows) = stage_with(4);
    windows[0].set_widget(true);
    stage.raise(&windows[0]); // widget on top: [1, 2, 3, 0]
    stage.set_fullscreen(
        "DP-1",
        windows[1].clone(),
        Point::from((0, 0)),
        Size::from((100, 100)),
    );

    stage.enforce_stacking();
    // Non-widgets keep relative order above the widget; fullscreen on top.
    assert_eq!(z_labels(&stage), vec![0, 2, 3, 1]);
    stage.verify_invariants();
}

#[test]
fn fit_round_trip_restores_saved_size() {
    let (mut stage, windows) = stage_with(1);
    let w = &windows[0];
    assert!(!stage.is_fit(w));

    stage.set_fit(w, Size::from((640, 480)));
    assert!(stage.is_fit(w));
    assert_eq!(stage.fit_saved_size(w), Some(Size::from((640, 480))));

    let saved = stage.take_fit_saved_size(w);
    assert_eq!(saved, Some(Size::from((640, 480))));
    assert!(!stage.is_fit(w));
    assert_eq!(stage.take_fit_saved_size(w), None);
}

#[test]
fn restore_size_if_missing_never_overwrites() {
    let (mut stage, windows) = stage_with(1);
    let w = &windows[0];
    stage.set_restore_size_if_missing(w, Size::from((300, 200)));
    stage.set_restore_size_if_missing(w, Size::from((999, 999)));
    assert_eq!(stage.restore_size(w), Some(Size::from((300, 200))));

    stage.set_restore_size(w, Size::from((500, 400)));
    assert_eq!(stage.restore_size(w), Some(Size::from((500, 400))));
}

#[test]
fn fullscreen_round_trip() {
    let (mut stage, windows) = stage_with(2);
    let w = &windows[0];
    stage.set_fullscreen(
        "DP-1",
        w.clone(),
        Point::from((10, 20)),
        Size::from((640, 480)),
    );

    assert!(stage.is_fullscreen(w));
    assert!(!stage.is_fullscreen(&windows[1]));
    assert_eq!(stage.fullscreen_output_of(w), Some("DP-1"));
    assert!(stage.has_fullscreen());
    stage.verify_invariants();

    let entry = stage.take_fullscreen("DP-1").unwrap();
    assert_eq!(entry.window, *w);
    assert_eq!(entry.saved_location, Point::from((10, 20)));
    assert_eq!(entry.saved_size, Size::from((640, 480)));
    assert!(!stage.has_fullscreen());
}

#[test]
fn retain_alive_drops_dead_windows_but_keeps_history() {
    let (mut stage, windows) = stage_with(2);
    stage.push_focus(&windows[0]);
    windows[0].kill();

    stage.retain_alive();
    assert!(!stage.contains(&windows[0]));
    assert!(stage.contains(&windows[1]));
    // Mirrors Space::refresh: history cleanup belongs to the destroy handlers.
    assert_eq!(stage.focus_history().len(), 1);
    stage.verify_invariants();
}

#[test]
fn replace_preserves_index_id_and_entry_state() {
    let (mut stage, windows) = stage_with(3);
    let old = &windows[1];
    let id = stage.id_of(old).unwrap();
    stage.set_restore_size(old, Size::from((640, 480)));
    stage.set_fit(old, Size::from((640, 480)));

    let new = TestWindow::new_suspended(99);
    stage.replace(old, new.clone());

    // Same z-slot (index 1), same ElementId, per-entry state carried over.
    assert_eq!(z_labels(&stage), vec![0, 99, 2]);
    assert_eq!(stage.id_of(&new), Some(id));
    assert!(!stage.contains(old));
    assert_eq!(stage.position_of(&new), Some(Point::from((10, 0))));
    assert_eq!(stage.restore_size(&new), Some(Size::from((640, 480))));
    assert!(stage.is_fit(&new));
    stage.verify_invariants();
}

#[test]
fn replace_ignores_unknown_window() {
    let (mut stage, windows) = stage_with(2);
    let unknown = TestWindow::new(42);
    stage.replace(&unknown, TestWindow::new_suspended(99));
    assert_eq!(z_labels(&stage), vec![0, 1]);
    assert!(stage.contains(&windows[0]));
    stage.verify_invariants();
}

#[test]
fn suspended_element_survives_retain_alive() {
    let (mut stage, windows) = stage_with(2);
    // Convert window 0 in place; its former client then dies.
    let suspended = TestWindow::new_suspended(99);
    stage.replace(&windows[0], suspended.clone());
    windows[0].kill();
    suspended.kill(); // the backing "client" is gone...

    stage.retain_alive();

    // ...but a suspended element stays alive until dismissed (removed).
    assert!(stage.contains(&suspended));
    assert!(stage.contains(&windows[1]));
    stage.verify_invariants();
}

#[test]
fn adoption_compound_keeps_slot_and_discards_fresh_id() {
    // By relaunch time the new client already owns a fresh stage entry, so
    // adoption is remove-the-fresh-entry + replace-the-suspended-one.
    let (mut stage, windows) = stage_with(3);
    let suspended = TestWindow::new_suspended(9);
    stage.replace(&windows[1], suspended.clone());
    let id = stage.id_of(&suspended).unwrap();

    let fresh = TestWindow::new(10);
    stage.map(fresh.clone(), Point::from((500, 500)));
    let fresh_id = stage.id_of(&fresh).unwrap();

    stage.remove(&fresh);
    stage.replace(&suspended, fresh.clone());

    // The adopted window takes the suspended slot, id, and position; the
    // fresh entry's id is gone for good (ids are never reused).
    assert_eq!(z_labels(&stage), vec![0, 10, 2]);
    assert_eq!(stage.id_of(&fresh), Some(id));
    assert_eq!(stage.position_of(&fresh), Some(Point::from((10, 0))));
    assert_eq!(stage.window_by_id(fresh_id), None);
    stage.verify_invariants();
}

#[test]
fn remove_from_history_matching_clamps_cycle() {
    let (mut stage, windows) = stage_with(3);
    for w in &windows {
        stage.push_focus(w);
    }
    cycle_step(&mut stage, false);
    cycle_step(&mut stage, false);
    assert_eq!(stage.cycle_state(), Some(2));

    let target = windows[0].clone();
    stage.remove_from_history_matching(|w| *w == target);
    assert_eq!(stage.cycle_state(), Some(1));
}

#[test]
fn raise_lifts_only_own_children() {
    // 0 has child 1; 2 is unrelated.
    let w: Vec<TestWindow> = (0..3).map(TestWindow::new).collect();
    w[1].set_parent(Some(w[0].clone()));
    let order = subtree_raise_order(&w, &w[0], |c, p| c.is_child_of(p));
    assert_eq!(labels(&order), vec![0, 1]);
    let order = subtree_raise_order(&w, &w[2], |c, p| c.is_child_of(p));
    assert_eq!(labels(&order), vec![2]);
}

#[test]
fn raise_follows_nested_modal_chain() {
    // 0 -> 1 -> 2 (dialog of a dialog), plus unrelated 3.
    let w: Vec<TestWindow> = (0..4).map(TestWindow::new).collect();
    w[1].set_parent(Some(w[0].clone()));
    w[2].set_parent(Some(w[1].clone()));
    let order = subtree_raise_order(&w, &w[0], |c, p| c.is_child_of(p));
    assert_eq!(labels(&order), vec![0, 1, 2]);
}

#[test]
fn raise_terminates_on_cyclic_parents() {
    // 0 and 1 claim each other as parent; must not loop forever.
    let w: Vec<TestWindow> = (0..2).map(TestWindow::new).collect();
    w[0].set_parent(Some(w[1].clone()));
    w[1].set_parent(Some(w[0].clone()));
    let order = subtree_raise_order(&w, &w[0], |c, p| c.is_child_of(p));
    assert_eq!(labels(&order), vec![0, 1]);
}

fn labels(order: &[TestWindow]) -> Vec<u64> {
    order.iter().map(|w| w.label()).collect()
}

/// Randomized op-sequence harness. Each op mirrors the sequence of stage
/// calls a `DriftWm` entry point performs (new_toplevel, toplevel_destroyed,
/// raise_and_focus, fullscreen enter/exit, fit toggle, MRU cycle, cluster
/// move/resize); `verify_invariants` runs after every op. Cluster ops drive
/// the real `cluster_of` / `adjacent_side` / `resolve_cluster_shifts`.
mod harness {
    use proptest::prelude::*;
    use smithay::utils::{Logical, Point, Size};
    use std::collections::{HashMap, HashSet};

    use crate::layout::auto_placement::{Rect, place_auto};
    use crate::layout::cluster::{self, ResizeClassification, Side};
    use crate::layout::snap::SnapRect;
    use crate::stage::mock::{SentConfigure, TestWindow};
    use crate::stage::{PinnedSite, Stage, StageElement};
    use crate::window_ext::WindowExt;

    const OUTPUTS: [&str; 3] = ["OUT-0", "OUT-1", "OUT-2"];
    const GAP: f64 = 6.0;
    /// Matches `adjacent_side`'s tolerance.
    const EPS: f64 = 1.0;

    #[derive(Debug, Clone)]
    enum Op {
        MapNew {
            x: i32,
            y: i32,
            w: i32,
            h: i32,
        },
        MapNewAutoPlaced {
            w: i32,
            h: i32,
            vx: i32,
            vy: i32,
        },
        CloseWindow {
            idx: usize,
        },
        CrashWindow {
            idx: usize,
        },
        Replace {
            idx: usize,
        },
        AdoptRelaunch {
            idx: usize,
        },
        DismissSuspended {
            idx: usize,
        },
        SetParent {
            child: usize,
            parent: usize,
        },
        MakeWidget {
            idx: usize,
        },
        MakeModal {
            idx: usize,
        },
        RaiseAndFocus {
            idx: usize,
        },
        HoverFocus {
            idx: usize,
        },
        ClickFocus {
            idx: usize,
        },
        MoveWindow {
            idx: usize,
            dx: i32,
            dy: i32,
        },
        SnapAdjacent {
            idx: usize,
            anchor: usize,
            side_sel: usize,
        },
        Cycle {
            backward: bool,
        },
        EndCycle,
        CancelCycle,
        EnterFullscreen {
            idx: usize,
            output: usize,
        },
        ExitFullscreen {
            output: usize,
        },
        TogglePin {
            idx: usize,
            output: usize,
        },
        RemoveOutput {
            output: usize,
        },
        AddOutput {
            output: usize,
        },
        ToggleFit {
            idx: usize,
        },
        ToggleFillMembership {
            idx: usize,
        },
        ResizeGrabEnd {
            idx: usize,
            w: i32,
            h: i32,
        },
        MoveCluster {
            idx: usize,
            dx: i32,
            dy: i32,
        },
        ResizeCluster {
            idx: usize,
            side_sel: usize,
            delta: i32,
        },
    }

    fn op_strategy() -> impl Strategy<Value = Op> {
        let idx = 0..64usize;
        prop_oneof![
            3 => (-2000..2000i32, -2000..2000i32, 50..500i32, 50..500i32)
                .prop_map(|(x, y, w, h)| Op::MapNew { x, y, w, h }),
            2 => (50..500i32, 50..500i32, -1000..1000i32, -1000..1000i32)
                .prop_map(|(w, h, vx, vy)| Op::MapNewAutoPlaced { w, h, vx, vy }),
            2 => idx.clone().prop_map(|idx| Op::CloseWindow { idx }),
            1 => idx.clone().prop_map(|idx| Op::CrashWindow { idx }),
            1 => idx.clone().prop_map(|idx| Op::Replace { idx }),
            1 => idx.clone().prop_map(|idx| Op::AdoptRelaunch { idx }),
            1 => idx.clone().prop_map(|idx| Op::DismissSuspended { idx }),
            1 => (idx.clone(), idx.clone()).prop_map(|(child, parent)| Op::SetParent { child, parent }),
            1 => idx.clone().prop_map(|idx| Op::MakeWidget { idx }),
            1 => idx.clone().prop_map(|idx| Op::MakeModal { idx }),
            3 => idx.clone().prop_map(|idx| Op::RaiseAndFocus { idx }),
            3 => idx.clone().prop_map(|idx| Op::HoverFocus { idx }),
            2 => idx.clone().prop_map(|idx| Op::ClickFocus { idx }),
            2 => (idx.clone(), -300..300i32, -300..300i32)
                .prop_map(|(idx, dx, dy)| Op::MoveWindow { idx, dx, dy }),
            3 => (idx.clone(), idx.clone(), 0..4usize)
                .prop_map(|(idx, anchor, side_sel)| Op::SnapAdjacent { idx, anchor, side_sel }),
            2 => any::<bool>().prop_map(|backward| Op::Cycle { backward }),
            1 => Just(Op::EndCycle),
            1 => Just(Op::CancelCycle),
            2 => (idx.clone(), 0..3usize)
                .prop_map(|(idx, output)| Op::EnterFullscreen { idx, output }),
            2 => (0..3usize).prop_map(|output| Op::ExitFullscreen { output }),
            2 => (idx.clone(), 0..3usize)
                .prop_map(|(idx, output)| Op::TogglePin { idx, output }),
            1 => (0..3usize).prop_map(|output| Op::RemoveOutput { output }),
            1 => (0..3usize).prop_map(|output| Op::AddOutput { output }),
            2 => idx.clone().prop_map(|idx| Op::ToggleFit { idx }),
            1 => idx.clone().prop_map(|idx| Op::ToggleFillMembership { idx }),
            1 => (idx.clone(), 50..500i32, 50..500i32)
                .prop_map(|(idx, w, h)| Op::ResizeGrabEnd { idx, w, h }),
            2 => (idx.clone(), -300..300i32, -300..300i32)
                .prop_map(|(idx, dx, dy)| Op::MoveCluster { idx, dx, dy }),
            2 => (idx, 0..4usize, -150..150i32)
                .prop_map(|(idx, side_sel, delta)| Op::ResizeCluster { idx, side_sel, delta }),
        ]
    }

    struct Sim {
        stage: Stage<TestWindow>,
        /// Every window ever created, including closed/crashed ones.
        windows: Vec<TestWindow>,
        /// The keyboard-focused window; tracked separately since not every
        /// focus target enters the history.
        focused: Option<TestWindow>,
        /// Expected pre-fit size per window label, for the restore assertion.
        fit_expect: HashMap<u64, Size<i32, Logical>>,
        /// Pin site saved when a pinned window fullscreens, restored on exit —
        /// the model of `FullscreenReturn::pinned`.
        pin_return: HashMap<String, PinnedSite>,
        next_label: u64,
        /// Live space outputs, mirroring the udev output registry.
        live: Vec<String>,
        /// The single virtual placeholder kept when the last output is
        /// removed (production's `disconnected_outputs`); pins/fullscreen may
        /// still target it.
        placeholder: Option<String>,
    }

    /// Plain window footprint (no SSD bar / border in the model). Suspended
    /// elements never snap or join clusters.
    fn rect_of(stage: &Stage<TestWindow>, w: &TestWindow) -> Option<SnapRect> {
        if w.is_widget() || w.is_suspended() || stage.is_fullscreen(w) || stage.is_pinned(w) {
            return None;
        }
        let pos = stage.position_of(w)?;
        let size = StageElement::size(w);
        Some(SnapRect {
            x_low: pos.x as f64,
            x_high: (pos.x + size.w) as f64,
            y_low: pos.y as f64,
            y_high: (pos.y + size.h) as f64,
        })
    }

    fn snap_rects(stage: &Stage<TestWindow>) -> Vec<(TestWindow, SnapRect)> {
        stage
            .windows()
            .filter_map(|w| rect_of(stage, w).map(|r| (w.clone(), r)))
            .collect()
    }

    fn overlaps(a: &SnapRect, b: &SnapRect) -> bool {
        a.x_low < b.x_high - EPS
            && b.x_low < a.x_high - EPS
            && a.y_low < b.y_high - EPS
            && b.y_low < a.y_high - EPS
    }

    /// Mirrors the modal walk in `DriftWm::cycle_anchor`.
    fn cycle_anchor(focused: &TestWindow) -> TestWindow {
        let mut window = focused.clone();
        for _ in 0..10 {
            if !window.is_modal() {
                break;
            }
            let Some(parent) = window.parent() else { break };
            window = parent;
        }
        window
    }

    /// True when following parent links from `w` loops back onto itself.
    /// `subtree_raise_order` tolerates such cycles by construction, but
    /// "child above parent" is unsatisfiable inside one, so the stacking
    /// assertion skips cycle members.
    fn in_parent_cycle(w: &TestWindow) -> bool {
        let mut seen = vec![w.clone()];
        let mut cur = w.clone();
        while let Some(p) = cur.parent() {
            if seen.contains(&p) {
                return true;
            }
            seen.push(p.clone());
            cur = p;
        }
        false
    }

    /// Innermost modal descendant, mirroring `DriftWm::topmost_modal_child`.
    fn topmost_modal_child(stage: &Stage<TestWindow>, w: &TestWindow) -> Option<TestWindow> {
        let mut current = w.clone();
        for _ in 0..10 {
            let child = stage
                .windows()
                .rev()
                .find(|c| c.is_child_of(&current) && c.is_modal())
                .cloned();
            match child {
                Some(c) => current = c,
                None => break,
            }
        }
        (current != *w).then_some(current)
    }

    impl Sim {
        fn new() -> Self {
            Sim {
                stage: Stage::new(),
                windows: Vec::new(),
                focused: None,
                fit_expect: HashMap::new(),
                pin_return: HashMap::new(),
                next_label: 0,
                live: OUTPUTS.iter().map(|o| o.to_string()).collect(),
                placeholder: None,
            }
        }

        /// The output-choosing ops' equivalent of `space.outputs()`: the live
        /// outputs plus the single virtual placeholder (itself a normal space
        /// output that fullscreen/pin may target).
        fn output_at(&self, sel: usize) -> String {
            let mut outs: Vec<&String> = self.live.iter().collect();
            if let Some(ph) = &self.placeholder {
                outs.push(ph);
            }
            outs[sel % outs.len()].clone()
        }

        /// Rebind every pin whose home output is no longer live to `to`,
        /// keeping `screen_pos` (the harness has no output sizes to clamp
        /// against). Mirrors `reassign_orphaned_pinned`: also rebinds pins
        /// suspended in fullscreen (`pin_return`, modeling
        /// `FullscreenReturn::pinned`), since a window pinned to output A but
        /// fullscreened on B would otherwise restore onto the dead A.
        fn reassign_orphaned_pinned(&mut self, to: &str) {
            let live = self.live.clone();
            let orphans: Vec<(TestWindow, PinnedSite)> = self
                .stage
                .pinned_windows()
                .filter(|(_, site)| !live.contains(&site.output))
                .map(|(w, site)| (w.clone(), site.clone()))
                .collect();
            let moved = !orphans.is_empty();
            for (w, mut site) in orphans {
                site.output = to.to_string();
                self.stage.set_pin(&w, site);
            }
            if moved {
                self.sync_pinned();
            }
            for site in self.pin_return.values_mut() {
                if !live.contains(&site.output) {
                    site.output = to.to_string();
                }
            }
        }

        /// Every output referenced by a fullscreen entry or a pin site must
        /// still be a live output or the virtual placeholder — output
        /// teardown/connect never strands stage state on a gone output.
        fn verify_outputs(&self) {
            let mapped = |name: &str| {
                self.live.iter().any(|o| o == name) || self.placeholder.as_deref() == Some(name)
            };
            for (output, _) in self.stage.fullscreen_entries() {
                assert!(
                    mapped(output),
                    "fullscreen entry on unmapped output {output}"
                );
            }
            for (_, site) in self.stage.pinned_windows() {
                assert!(
                    mapped(&site.output),
                    "pinned window on unmapped output {}",
                    site.output
                );
            }
            for (output, site) in &self.pin_return {
                assert!(
                    mapped(&site.output),
                    "pin suspended by fullscreen on {output} on unmapped output {}",
                    site.output
                );
            }
        }

        /// Suspended elements never hold fullscreen or pin membership and
        /// never enter the MRU history (the pinned-window pattern).
        fn verify_suspended(&self) {
            for w in self.stage.windows() {
                if w.is_suspended() {
                    assert!(!self.stage.is_pinned(w), "suspended element pinned");
                }
            }
            for (output, fs) in self.stage.fullscreen_entries() {
                assert!(
                    !fs.window.is_suspended(),
                    "suspended element fullscreen on {output}"
                );
            }
            for w in self.stage.focus_history() {
                assert!(!w.is_suspended(), "suspended element in focus history");
            }
        }

        fn pick(&self, idx: usize) -> Option<TestWindow> {
            (!self.windows.is_empty()).then(|| self.windows[idx % self.windows.len()].clone())
        }

        /// Mirrors `DriftWm::raise_and_focus` + the `focus_changed` history push.
        /// Returns the window pushed to the MRU, if any.
        fn raise_and_focus(&mut self, w: &TestWindow) -> Option<TestWindow> {
            let order = self.stage.raise_with_children(w);
            self.stage.enforce_stacking();

            // The modal-dialog stacking guard: within the raised subtree, every
            // child sits above its own parent (fullscreen/widget re-stacking
            // exempted).
            let z: Vec<TestWindow> = self.stage.windows().cloned().collect();
            let idx_of = |x: &TestWindow| z.iter().position(|y| y == x);
            for child in &order {
                for parent in &order {
                    if child.is_child_of(parent)
                        && !in_parent_cycle(child)
                        && !child.is_widget()
                        && !parent.is_widget()
                        && !self.stage.is_fullscreen(child)
                        && !self.stage.is_fullscreen(parent)
                        && let (Some(ci), Some(pi)) = (idx_of(child), idx_of(parent))
                    {
                        assert!(ci > pi, "child stacked below its parent after raise");
                    }
                }
            }

            self.push_focus_as_focus_changed_would(w)
        }

        /// Mirror of the `focus_changed` → `update_focus_history` chain. Pushes
        /// to the real stage and returns the pushed window, if any; keyboard
        /// focus (the cycle anchor) moves regardless of push eligibility.
        ///
        /// Focus-*change* gating is not modeled — the real chain only pushes
        /// when keyboard focus actually changes — but the push is idempotent
        /// at the MRU head, so the difference is unobservable here.
        fn push_focus_as_focus_changed_would(&mut self, w: &TestWindow) -> Option<TestWindow> {
            let focused = topmost_modal_child(&self.stage, w).unwrap_or_else(|| w.clone());
            let eligible = self.stage.cycle_state().is_none()
                && !focused.is_widget()
                && !focused.is_suspended()
                && !focused.is_modal()
                && !self.stage.is_pinned(&focused);
            let pushed = eligible.then(|| {
                self.stage.push_focus(&focused);
                focused.clone()
            });
            self.focused = Some(cycle_anchor(&focused));
            pushed
        }

        /// A dead window loses focus; production then refocuses the history
        /// head, which is where a fresh cycle starts from anyway.
        fn clear_focus_if(&mut self, dead: &TestWindow) {
            if self.focused.as_ref() == Some(dead) {
                self.focused = None;
            }
        }

        /// Mirrors `DriftWm::exit_fullscreen_on`'s stage half, including the
        /// pin restore and the pinned-loc sync (which re-maps every pinned
        /// window, raising it) that follows.
        fn exit_fullscreen(&mut self, output: &str) {
            if let Some(entry) = self.stage.take_fullscreen(output) {
                entry.window.exit_fullscreen_configure(entry.saved_size);
                self.stage.map(entry.window.clone(), entry.saved_location);
                if let Some(site) = self.pin_return.remove(output) {
                    // The window may have entered the MRU while fullscreen;
                    // re-pinning takes it back out (as the exit path does).
                    self.stage.drop_from_focus_history(&entry.window);
                    self.stage.set_pin(&entry.window, site);
                    self.sync_pinned();
                }
            }
        }

        /// Mirrors `sync_pinned_locs`: re-map every pinned window in z-order
        /// (each map raises, so relative pinned stacking is preserved).
        fn sync_pinned(&mut self) {
            let pinned: Vec<TestWindow> = self
                .stage
                .pinned_windows()
                .map(|(w, _)| w.clone())
                .collect();
            for w in pinned {
                let Some(pos) = self.stage.position_of(&w) else {
                    continue;
                };
                self.stage.map(w, pos);
            }
        }

        /// Drive the real `place_auto` for a new window and assert its
        /// contract: overlap-free against every obstacle rect (ineligible
        /// windows included) and snap-adjacent to the anchor's cluster.
        /// Deliberately wider than production's `place_adjacent_to`, which
        /// drops ineligible windows from the obstacle list entirely — kept
        /// here to exercise that half of the interface. Falls back to
        /// `(vx, vy)` when there's no eligible anchor or no fit.
        #[allow(clippy::mutable_key_type)]
        fn auto_place(&self, new_w: i32, new_h: i32, vx: i32, vy: i32) -> (i32, i32) {
            let Some(anchor) = self.stage.focus_history().first().cloned() else {
                return (vx, vy);
            };
            if !self.stage.contains(&anchor) {
                return (vx, vy);
            }

            let mut rects: Vec<Rect> = Vec::new();
            let mut eligible: HashSet<usize> = HashSet::new();
            let mut eligible_rects: Vec<(TestWindow, SnapRect)> = Vec::new();
            let mut focused_idx = None;
            for win in self.stage.windows() {
                let Some(pos) = self.stage.position_of(win) else {
                    continue;
                };
                let size = StageElement::size(win);
                let idx = rects.len();
                rects.push(Rect {
                    x: pos.x as f64,
                    y: pos.y as f64,
                    w: size.w as f64,
                    h: size.h as f64,
                });
                if *win == anchor {
                    focused_idx = Some(idx);
                }
                if !win.is_widget()
                    && !win.is_suspended()
                    && !self.stage.is_fullscreen(win)
                    && !self.stage.is_pinned(win)
                {
                    eligible.insert(idx);
                    eligible_rects.push((
                        win.clone(),
                        SnapRect {
                            x_low: pos.x as f64,
                            x_high: (pos.x + size.w) as f64,
                            y_low: pos.y as f64,
                            y_high: (pos.y + size.h) as f64,
                        },
                    ));
                }
            }
            let Some(focused_idx) = focused_idx else {
                return (vx, vy);
            };

            let Some((px, py)) = place_auto(
                &rects,
                focused_idx,
                &eligible,
                new_w as f64,
                new_h as f64,
                (vx as f64, vy as f64),
                GAP,
            ) else {
                return (vx, vy);
            };

            let placed = SnapRect {
                x_low: px,
                x_high: px + new_w as f64,
                y_low: py,
                y_high: py + new_h as f64,
            };
            // place_auto's forbidden-interval logic must clear the new frame
            // of every obstacle, not just its own cluster.
            for r in &rects {
                let obstacle = SnapRect {
                    x_low: r.x,
                    x_high: r.x + r.w,
                    y_low: r.y,
                    y_high: r.y + r.h,
                };
                assert!(
                    !overlaps(&placed, &obstacle),
                    "auto-placed window overlaps an existing window"
                );
            }
            let community = cluster::cluster_of(&anchor, &eligible_rects, GAP);
            assert!(
                eligible_rects.iter().any(|(m, r)| community.contains(m)
                    && cluster::adjacent_side(&placed, r, GAP).is_some()),
                "auto-placed window not adjacent to the anchor's cluster"
            );

            (px.round() as i32, py.round() as i32)
        }

        // TestWindow hashes by Rc pointer identity (stable under interior
        // mutation), so clippy's mutable-key-type lint is a false positive —
        // same rationale as production's smithay::Window keys.
        #[allow(clippy::mutable_key_type)]
        fn apply(&mut self, op: &Op) {
            match op {
                Op::MapNew { x, y, w, h } => {
                    let win = TestWindow::new(self.next_label);
                    self.next_label += 1;
                    win.set_size(Size::from((*w, *h)));
                    // new_toplevel: map + raise + enforce; focus comes later
                    // (first commit), modeled by RaiseAndFocus ops.
                    self.stage.map(win.clone(), Point::from((*x, *y)));
                    self.stage.raise(&win);
                    self.stage.enforce_stacking();
                    self.windows.push(win);
                }
                Op::MapNewAutoPlaced { w, h, vx, vy } => {
                    // new_toplevel with placement = "auto": place adjacent to
                    // the focused window's cluster via the real place_auto.
                    let win = TestWindow::new(self.next_label);
                    self.next_label += 1;
                    win.set_size(Size::from((*w, *h)));
                    let pos = self.auto_place(*w, *h, *vx, *vy);
                    self.stage.map(win.clone(), Point::from(pos));
                    self.stage.raise(&win);
                    self.stage.enforce_stacking();
                    self.windows.push(win);
                }
                Op::CloseWindow { idx } => {
                    let Some(w) = self.pick(*idx) else { return };
                    // toplevel_destroyed: fullscreen teardown first, then
                    // unmap. The whole return payload is consumed — including
                    // a saved pin site, which dies with the window.
                    if let Some(out) = self.stage.fullscreen_output_of(&w).map(str::to_owned) {
                        self.stage.take_fullscreen(&out);
                        self.pin_return.remove(&out);
                    }
                    w.kill();
                    self.stage.remove(&w);
                    self.fit_expect.remove(&w.label());
                    self.clear_focus_if(&w);
                }
                Op::CrashWindow { idx } => {
                    // Crash path: the destroy handlers purge the history and
                    // reap any fullscreen entry the dead window left behind;
                    // Space::refresh (mirrored by retain_alive) drops the
                    // window from the z-order.
                    let Some(w) = self.pick(*idx) else { return };
                    w.kill();
                    self.stage.remove_from_history_matching(|x| x == &w);
                    if let Some(out) = self.stage.fullscreen_output_of(&w).map(str::to_owned) {
                        self.stage.take_fullscreen(&out);
                        self.pin_return.remove(&out);
                    }
                    let suspended_before: Vec<TestWindow> = self
                        .stage
                        .windows()
                        .filter(|x| x.is_suspended())
                        .cloned()
                        .collect();
                    self.stage.retain_alive();
                    // The idle-tick sweep must never cull a suspended entry —
                    // they have no surface to die and leave only on dismissal.
                    for s in &suspended_before {
                        assert!(
                            self.stage.contains(s),
                            "retain_alive culled a suspended entry"
                        );
                    }
                    self.clear_focus_if(&w);
                }
                Op::Replace { idx } => {
                    // Models the suspend conversion: client torn down, its slot
                    // replaced in place by a surfaceless element. `replace`
                    // doesn't touch focus-history/fullscreen, so we clear those
                    // first, mirroring the real caller's responsibility.
                    let Some(old) = self.pick(*idx) else { return };
                    if !self.stage.contains(&old) || old.is_suspended() {
                        return;
                    }
                    if let Some(out) = self.stage.fullscreen_output_of(&old).map(str::to_owned) {
                        self.stage.take_fullscreen(&out);
                        self.pin_return.remove(&out);
                    }
                    self.stage.take_pin(&old);
                    self.stage.clear_fit(&old);
                    self.fit_expect.remove(&old.label());

                    let id_before = self.stage.id_of(&old).unwrap();
                    let idx_before = self.stage.windows().position(|w| *w == old).unwrap();

                    let suspended = TestWindow::new_suspended(self.next_label);
                    self.next_label += 1;
                    self.stage.replace(&old, suspended.clone());

                    old.kill();
                    self.stage.remove_from_history_matching(|w| *w == old);
                    self.clear_focus_if(&old);
                    self.windows.push(suspended.clone());

                    assert_eq!(
                        self.stage.id_of(&suspended),
                        Some(id_before),
                        "replace changed the ElementId"
                    );
                    assert_eq!(
                        self.stage.windows().position(|w| *w == suspended),
                        Some(idx_before),
                        "replace changed the z-order index"
                    );
                    assert!(!self.stage.contains(&old), "old element survived replace");
                }
                Op::AdoptRelaunch { idx } => {
                    // Compound adoption: by relaunch time the new client
                    // already owns a fresh stage entry (mapped at
                    // new_toplevel), so a bare replace would duplicate it.
                    // Adopt = remove the fresh entry (its id is discarded),
                    // then replace the suspended one in place.
                    let suspended: Vec<TestWindow> = self
                        .stage
                        .windows()
                        .filter(|w| w.is_suspended())
                        .cloned()
                        .collect();
                    if suspended.is_empty() {
                        return;
                    }
                    let target = suspended[idx % suspended.len()].clone();

                    let fresh = TestWindow::new(self.next_label);
                    self.next_label += 1;
                    fresh.set_size(StageElement::size(&target));
                    self.stage.map(fresh.clone(), Point::from((0, 0)));
                    self.stage.raise(&fresh);
                    self.stage.enforce_stacking();
                    let fresh_id = self.stage.id_of(&fresh).unwrap();

                    let id_before = self.stage.id_of(&target).unwrap();
                    let idx_before = self.stage.windows().position(|w| *w == target).unwrap();

                    self.stage.remove(&fresh);
                    self.stage.replace(&target, fresh.clone());
                    self.clear_focus_if(&target);
                    self.windows.push(fresh.clone());

                    assert_eq!(
                        self.stage.id_of(&fresh),
                        Some(id_before),
                        "adoption changed the ElementId"
                    );
                    assert_eq!(
                        self.stage.windows().position(|w| *w == fresh),
                        Some(idx_before),
                        "adoption changed the z-order index"
                    );
                    assert!(
                        self.stage.window_by_id(fresh_id).is_none(),
                        "the fresh entry's discarded id still resolves"
                    );
                }
                Op::DismissSuspended { idx } => {
                    // Closing a suspended window dismisses it — plain removal,
                    // the only way a suspended entry leaves the stage.
                    let suspended: Vec<TestWindow> = self
                        .stage
                        .windows()
                        .filter(|w| w.is_suspended())
                        .cloned()
                        .collect();
                    if suspended.is_empty() {
                        return;
                    }
                    let target = suspended[idx % suspended.len()].clone();
                    self.stage.remove(&target);
                    self.clear_focus_if(&target);
                }
                Op::SetParent { child, parent } => {
                    // Suspended elements carry no xdg parent links.
                    let (Some(c), Some(p)) = (self.pick(*child), self.pick(*parent)) else {
                        return;
                    };
                    if c != p && !c.is_suspended() && !p.is_suspended() {
                        c.set_parent(Some(p));
                    }
                }
                Op::MakeWidget { idx } => {
                    // Widget rule applied on first commit: drop from history,
                    // re-assert stacking. Rules need a surface — suspended
                    // elements can't become widgets.
                    let Some(w) = self.pick(*idx) else { return };
                    if w.is_suspended() {
                        return;
                    }
                    w.set_widget(true);
                    self.stage.drop_from_focus_history(&w);
                    self.stage.enforce_stacking();
                }
                Op::MakeModal { idx } => {
                    let Some(w) = self.pick(*idx) else { return };
                    if w.is_suspended() {
                        return;
                    }
                    w.set_modal(true);
                }
                Op::RaiseAndFocus { idx } => {
                    let Some(w) = self.pick(*idx) else { return };
                    if !self.stage.contains(&w) {
                        return;
                    }
                    self.raise_and_focus(&w.clone());
                }
                Op::HoverFocus { idx } => {
                    // maybe_hover_focus (focus-follows-mouse): sloppy focus that
                    // never raises. idx is the hit-test result — "this window
                    // is under the cursor" — because the harness models focus
                    // policy, not canvas geometry. The only push Op that
                    // leaves the z-stack untouched outside cycling.
                    let Some(w) = self.pick(*idx) else { return };
                    if !self.stage.contains(&w) || w.is_widget() {
                        return;
                    }
                    let z_before: Vec<TestWindow> = self.stage.windows().cloned().collect();
                    if let Some(target) = self.push_focus_as_focus_changed_would(&w.clone()) {
                        assert_eq!(
                            self.stage.focus_history().first(),
                            Some(&target),
                            "hover-focus left the hovered window off the MRU head"
                        );
                    }
                    assert_eq!(
                        z_before,
                        self.stage.windows().cloned().collect::<Vec<_>>(),
                        "hover-focus reordered the z-stack"
                    );
                }
                Op::ClickFocus { idx } => {
                    // on_pointer_button click-to-focus: unlike HoverFocus, this
                    // raises the target to z-top in addition to pushing the MRU.
                    // A widget click is focus-only: keyboard focus (the cycle
                    // anchor) moves, but there is no raise and no MRU entry.
                    let Some(w) = self.pick(*idx) else { return };
                    if !self.stage.contains(&w) {
                        return;
                    }
                    if w.is_widget() {
                        self.focused = Some(cycle_anchor(&w));
                        return;
                    }
                    let history_before: Vec<TestWindow> = self.stage.focus_history().to_vec();
                    match self.raise_and_focus(&w.clone()) {
                        Some(target) => assert_eq!(
                            self.stage.focus_history().first(),
                            Some(&target),
                            "click-focus left the clicked window off the MRU head"
                        ),
                        // The raise machinery (subtree raise, enforce_stacking,
                        // pinned re-sync) must not touch the history when the
                        // target is ineligible for a push.
                        None => assert_eq!(
                            history_before,
                            self.stage.focus_history().to_vec(),
                            "click-focus of an ineligible target mutated the MRU"
                        ),
                    }
                }
                Op::MoveWindow { idx, dx, dy } => {
                    // NudgeWindow: canvas windows only.
                    let Some(w) = self.pick(*idx) else { return };
                    if w.is_widget()
                        || w.is_suspended()
                        || self.stage.is_fullscreen(&w)
                        || self.stage.is_pinned(&w)
                    {
                        return;
                    }
                    let Some(pos) = self.stage.position_of(&w) else {
                        return;
                    };
                    self.stage.map(w, pos + Point::from((*dx, *dy)));
                }
                Op::SnapAdjacent {
                    idx,
                    anchor,
                    side_sel,
                } => {
                    // Place `idx` flush against `anchor` with the snap gap, as
                    // a drag-snap would — this is what forms clusters.
                    let (Some(w), Some(a)) = (self.pick(*idx), self.pick(*anchor)) else {
                        return;
                    };
                    if w == a
                        || w.is_widget()
                        || w.is_suspended()
                        || self.stage.is_fullscreen(&w)
                        || self.stage.is_pinned(&w)
                    {
                        return;
                    }
                    let Some(anchor_rect) = rect_of(&self.stage, &a) else {
                        return;
                    };
                    if !self.stage.contains(&w) {
                        return;
                    }
                    let size = StageElement::size(&w);
                    let side = [Side::Right, Side::Left, Side::Bottom, Side::Top][side_sel % 4];
                    let pos = match side {
                        Side::Right => {
                            ((anchor_rect.x_high + GAP) as i32, anchor_rect.y_low as i32)
                        }
                        Side::Left => (
                            (anchor_rect.x_low - GAP) as i32 - size.w,
                            anchor_rect.y_low as i32,
                        ),
                        Side::Bottom => {
                            (anchor_rect.x_low as i32, (anchor_rect.y_high + GAP) as i32)
                        }
                        Side::Top => (
                            anchor_rect.x_low as i32,
                            (anchor_rect.y_low - GAP) as i32 - size.h,
                        ),
                    };
                    self.stage.map(w, Point::from(pos));
                }
                Op::Cycle { backward } => {
                    let focused = self.focused.clone();
                    if let Some(target) = self.stage.cycle_step(*backward, focused.as_ref()) {
                        // navigate_to_window → raise_and_focus; the history
                        // push is skipped while cycling (focus_changed guard).
                        self.raise_and_focus(&target);
                    }
                }
                Op::EndCycle => self.stage.end_cycle(),
                Op::CancelCycle => self.stage.cancel_cycle(),
                Op::EnterFullscreen { idx, output } => {
                    let Some(w) = self.pick(*idx) else { return };
                    if w.is_widget() || w.is_suspended() || !w.is_alive() {
                        return;
                    }
                    let key = self.output_at(*output);
                    // Idempotent re-assert keeps the existing saved geometry.
                    if self
                        .stage
                        .fullscreen_on(&key)
                        .is_some_and(|fs| fs.window == w)
                    {
                        w.enter_fullscreen_configure(Size::from((1920, 1080)));
                        return;
                    }
                    if let Some(other) = self.stage.fullscreen_output_of(&w).map(str::to_owned)
                        && other != key
                    {
                        self.exit_fullscreen(&other);
                    }
                    if self.stage.fullscreen_on(&key).is_some() {
                        self.exit_fullscreen(&key);
                    }
                    let saved_size = if self.stage.is_fit(&w) {
                        StageElement::size(&w)
                    } else {
                        self.stage
                            .restore_size(&w)
                            .unwrap_or_else(|| StageElement::size(&w))
                    };
                    let saved_location = self.stage.position_of(&w).unwrap_or_default();
                    // Unpin into fullscreen; the exit restores the site.
                    if let Some(site) = self.stage.take_pin(&w) {
                        self.pin_return.insert(key.clone(), site);
                    }
                    self.stage
                        .set_fullscreen(&key, w.clone(), saved_location, saved_size);
                    w.enter_fullscreen_configure(Size::from((1920, 1080)));
                    self.stage.map(w.clone(), Point::from((0, 0)));
                    self.stage.raise(&w);
                    self.stage.enforce_stacking();
                }
                Op::ExitFullscreen { output } => {
                    let key = self.output_at(*output);
                    self.exit_fullscreen(&key);
                }
                Op::ToggleFit { idx } => {
                    let Some(w) = self.pick(*idx) else { return };
                    // Models the keybinding path, which filters fullscreen
                    // windows out. The client-initiated maximize path has no
                    // such guard — a quirk this harness deliberately does
                    // not cover.
                    if w.is_widget()
                        || w.is_suspended()
                        || self.stage.is_fullscreen(&w)
                        || self.stage.is_pinned(&w)
                        || !self.stage.contains(&w)
                    {
                        return;
                    }
                    let Some(pos) = self.stage.position_of(&w) else {
                        return;
                    };
                    if self.stage.is_fit(&w) {
                        let saved = self.stage.take_fit_saved_size(&w).unwrap();
                        // The pre-fit-restore guarantee: unfit configures the
                        // exact size saved when the window was fit.
                        assert_eq!(
                            Some(saved),
                            self.fit_expect.remove(&w.label()),
                            "unfit restored a different size than was saved at fit time"
                        );
                        w.exit_fit_configure(saved);
                        self.stage.map(w, pos);
                    } else {
                        let current = self
                            .stage
                            .restore_size(&w)
                            .unwrap_or_else(|| StageElement::size(&w));
                        self.stage.set_fit(&w, current);
                        self.fit_expect.insert(w.label(), current);
                        w.enter_fit_configure(Size::from((1000, 700)));
                        self.stage.map(w.clone(), pos);
                        self.raise_and_focus(&w);
                    }
                }
                Op::ToggleFillMembership { idx } => {
                    // Fill's geometry lives in DriftWm, not the stage; here we
                    // only exercise the membership field so verify_invariants
                    // covers it. Same eligibility as the keybinding path.
                    let Some(w) = self.pick(*idx) else { return };
                    if w.is_widget()
                        || self.stage.is_fullscreen(&w)
                        || self.stage.is_pinned(&w)
                        || self.stage.is_fit(&w)
                        || !self.stage.contains(&w)
                    {
                        return;
                    }
                    if self.stage.is_fill(&w) {
                        self.stage.take_fill_saved(&w);
                    } else {
                        let pos = self.stage.position_of(&w).unwrap_or_default();
                        let size = StageElement::size(&w);
                        self.stage.set_fill(&w, pos, size);
                    }
                }
                Op::ResizeGrabEnd { idx, w: nw, h: nh } => {
                    // End of a user resize: fit cleared at grab start, size
                    // committed, restore size anchored to the user's choice.
                    let Some(w) = self.pick(*idx) else { return };
                    if !self.stage.contains(&w) {
                        return;
                    }
                    // A suspended resize only updates the element's size — no
                    // fit/restore bookkeeping (there is no client to configure).
                    if w.is_suspended() {
                        w.set_size(Size::from((*nw, *nh)));
                        return;
                    }
                    self.stage.clear_fit(&w);
                    self.fit_expect.remove(&w.label());
                    w.set_size(Size::from((*nw, *nh)));
                    self.stage.set_restore_size(&w, Size::from((*nw, *nh)));
                }
                Op::TogglePin { idx, output } => {
                    // Mirrors toggle_pin_to_screen: unpin remaps at the
                    // derived canvas spot (raising); pin drops the window from
                    // the MRU history first.
                    let Some(w) = self.pick(*idx) else { return };
                    if !self.stage.contains(&w) || w.is_suspended() || self.stage.is_fullscreen(&w)
                    {
                        return;
                    }
                    if self.stage.take_pin(&w).is_some() {
                        let pos = self.stage.position_of(&w).unwrap_or_default();
                        self.stage.map(w, pos);
                    } else {
                        let out = self.output_at(*output);
                        self.stage.drop_from_focus_history(&w);
                        self.stage.set_pin(
                            &w,
                            PinnedSite {
                                output: out,
                                screen_pos: Point::from((10, 10)),
                            },
                        );
                    }
                }
                Op::RemoveOutput { output } => {
                    // teardown_output: exit fullscreen on the gone output, then
                    // either keep it as the last-output placeholder (pins stay)
                    // or unmap it and rebind orphaned pins to a survivor.
                    if self.live.is_empty() {
                        return;
                    }
                    let name = self.live[*output % self.live.len()].clone();
                    self.exit_fullscreen(&name);
                    self.live.retain(|o| o != &name);
                    if self.live.is_empty() {
                        self.placeholder = Some(name);
                    } else {
                        let to = self.live[0].clone();
                        self.reassign_orphaned_pinned(&to);
                    }
                }
                Op::AddOutput { output } => {
                    // Connect handler: swap the placeholder for the new output
                    // (exiting any fullscreen entered on it), then rebind pins
                    // orphaned by the swap to the new output.
                    let name = OUTPUTS[*output % OUTPUTS.len()].to_string();
                    if self.live.contains(&name) {
                        return;
                    }
                    if let Some(ph) = self.placeholder.take() {
                        self.exit_fullscreen(&ph);
                        self.live.push(name.clone());
                        self.reassign_orphaned_pinned(&name);
                    } else {
                        self.live.push(name);
                    }
                }
                Op::MoveCluster { idx, dx, dy } => {
                    let Some(w) = self.pick(*idx) else { return };
                    if w.is_widget()
                        || w.is_suspended()
                        || self.stage.is_fullscreen(&w)
                        || self.stage.is_pinned(&w)
                    {
                        return;
                    }
                    let Some(primary_pos) = self.stage.position_of(&w) else {
                        return;
                    };
                    let rects = snap_rects(&self.stage);
                    let component = cluster::cluster_of(&w, &rects, GAP);
                    let members: Vec<(TestWindow, Point<i32, Logical>)> = component
                        .iter()
                        .filter(|m| *m != &w)
                        .filter_map(|m| {
                            self.stage
                                .position_of(m)
                                .map(|p| (m.clone(), p - primary_pos))
                        })
                        .collect();
                    let new_loc = primary_pos + Point::from((*dx, *dy));
                    // Members first, primary last — it stays on top of its
                    // own cluster (move_grab's ordering invariant).
                    for (m, off) in &members {
                        self.stage.map(m.clone(), new_loc + *off);
                    }
                    self.stage.map(w.clone(), new_loc);

                    // A cluster drag is rigid: offsets survive verbatim.
                    for (m, off) in &members {
                        assert_eq!(
                            self.stage.position_of(m).unwrap()
                                - self.stage.position_of(&w).unwrap(),
                            *off,
                            "cluster drag broke a member offset"
                        );
                    }
                    let top: Vec<&TestWindow> = self.stage.windows().rev().take(1).collect();
                    assert_eq!(top[0], &w, "cluster drag left the primary below a member");
                }
                Op::ResizeCluster {
                    idx,
                    side_sel,
                    delta,
                } => {
                    self.resize_cluster(*idx, *side_sel, *delta);
                }
            }
        }

        /// One resize-grab motion tick against the primary's `side` edge,
        /// classification mirroring `cluster_snapshot_for_resize` and shifts
        /// from the real `resolve_cluster_shifts`.
        //
        // Same Rc-identity rationale as `apply` for the key-type lint.
        #[allow(clippy::mutable_key_type)]
        fn resize_cluster(&mut self, idx: usize, side_sel: usize, delta: i32) {
            let Some(w) = self.pick(idx) else { return };
            if w.is_widget()
                || w.is_suspended()
                || self.stage.is_fullscreen(&w)
                || self.stage.is_pinned(&w)
            {
                return;
            }
            let Some(primary_rect) = rect_of(&self.stage, &w) else {
                return;
            };
            let Some(primary_pos) = self.stage.position_of(&w) else {
                return;
            };
            let size = StageElement::size(&w);
            let side = [Side::Right, Side::Left, Side::Bottom, Side::Top][side_sel % 4];
            let horizontal = matches!(side, Side::Right | Side::Left);
            // Keep the window at a sane minimum so shrink deltas stay valid.
            let delta = if horizontal {
                delta.max(20 - size.w)
            } else {
                delta.max(20 - size.h)
            };
            if delta == 0 {
                return;
            }
            let (width_delta, height_delta) = if horizontal { (delta, 0) } else { (0, delta) };

            let rects = snap_rects(&self.stage);
            let full = cluster::cluster_of(&w, &rects, GAP);
            let rect_lookup: HashMap<TestWindow, SnapRect> = rects.iter().cloned().collect();

            // Shift set: direct neighbors on `side`, then BFS through the
            // cluster adding downstream members (cluster_snapshot_for_resize).
            let is_downstream = |r: &SnapRect| match side {
                Side::Right => r.x_low >= primary_rect.x_high,
                Side::Left => r.x_high <= primary_rect.x_low,
                Side::Bottom => r.y_low >= primary_rect.y_high,
                Side::Top => r.y_high <= primary_rect.y_low,
            };
            let mut shift_set: HashSet<TestWindow> = HashSet::new();
            let mut queue: Vec<TestWindow> = Vec::new();
            for (m, r) in &rects {
                if m != &w && cluster::adjacent_side(&primary_rect, r, GAP) == Some(side) {
                    shift_set.insert(m.clone());
                    queue.push(m.clone());
                }
            }
            let direct_neighbors: Vec<TestWindow> = queue.clone();
            while let Some(current) = queue.pop() {
                let Some(cur_rect) = rect_lookup.get(&current) else {
                    continue;
                };
                for (m, r) in &rects {
                    if m == &w || shift_set.contains(m) || !full.contains(m) {
                        continue;
                    }
                    if cluster::adjacent_side(cur_rect, r, GAP).is_some() && is_downstream(r) {
                        shift_set.insert(m.clone());
                        queue.push(m.clone());
                    }
                }
            }

            let members: Vec<(TestWindow, Point<i32, Logical>, SnapRect)> = full
                .iter()
                .filter(|m| *m != &w)
                .filter_map(|m| {
                    let pos = self.stage.position_of(m)?;
                    let rect = rect_lookup.get(m)?;
                    Some((m.clone(), pos, *rect))
                })
                .collect();
            let classifications: Vec<ResizeClassification> = members
                .iter()
                .map(|(m, _, rect)| ResizeClassification {
                    axis_x: (horizontal && shift_set.contains(m)).then_some(side),
                    axis_y: (!horizontal && shift_set.contains(m)).then_some(side),
                    initial_rect: *rect,
                })
                .collect();

            let mut primary_cur = primary_rect;
            match side {
                Side::Right => primary_cur.x_high += width_delta as f64,
                Side::Left => primary_cur.x_low -= width_delta as f64,
                Side::Bottom => primary_cur.y_high += height_delta as f64,
                Side::Top => primary_cur.y_low -= height_delta as f64,
            }

            let (shifts, _bonds) = cluster::resolve_cluster_shifts(
                &classifications,
                width_delta,
                height_delta,
                GAP,
                &[],
                Some((primary_rect, primary_cur)),
            );

            // Record which pairs were overlap-free before the op; the reflow
            // must not create new overlaps among them.
            let clean_before: HashSet<(u64, u64)> = {
                let mut all: Vec<(TestWindow, SnapRect)> =
                    members.iter().map(|(m, _, r)| (m.clone(), *r)).collect();
                all.push((w.clone(), primary_rect));
                let mut clean = HashSet::new();
                for (i, (a, ra)) in all.iter().enumerate() {
                    for (b, rb) in all.iter().skip(i + 1) {
                        if !overlaps(ra, rb) {
                            clean.insert((a.label().min(b.label()), a.label().max(b.label())));
                        }
                    }
                }
                clean
            };

            // Apply: members first, primary re-mapped last (apply_member_shifts).
            for (i, (dx, dy)) in &shifts {
                let (m, initial_pos, _) = &members[*i];
                if !m.is_alive() {
                    continue;
                }
                self.stage
                    .map(m.clone(), *initial_pos + Point::from((*dx, *dy)));
            }
            // The primary's size/position change lands via the client commit.
            let (new_pos, new_size) = match side {
                Side::Right => (primary_pos, Size::from((size.w + delta, size.h))),
                Side::Left => (
                    primary_pos - Point::from((delta, 0)),
                    Size::from((size.w + delta, size.h)),
                ),
                Side::Bottom => (primary_pos, Size::from((size.w, size.h + delta))),
                Side::Top => (
                    primary_pos - Point::from((0, delta)),
                    Size::from((size.w, size.h + delta)),
                ),
            };
            w.set_size(new_size);
            self.stage.map(w.clone(), new_pos);

            // The reflow invariant, part 1: members directly snapped to the
            // resized edge stay flush with it — same side, same gap.
            let new_primary_rect = rect_of(&self.stage, &w).unwrap();
            for m in &direct_neighbors {
                let Some(m_rect) = rect_of(&self.stage, m) else {
                    continue;
                };
                assert_eq!(
                    cluster::adjacent_side(&new_primary_rect, &m_rect, GAP),
                    Some(side),
                    "resize reflow broke a direct neighbor's snap gap"
                );
            }

            // Part 2: no overlap appears between pairs that were clean before
            // (the cascade-convergence guarantee).
            let mut all_after: Vec<(TestWindow, SnapRect)> = members
                .iter()
                .filter_map(|(m, _, _)| rect_of(&self.stage, m).map(|r| (m.clone(), r)))
                .collect();
            all_after.push((w.clone(), new_primary_rect));
            for (i, (a, ra)) in all_after.iter().enumerate() {
                for (b, rb) in all_after.iter().skip(i + 1) {
                    let key = (a.label().min(b.label()), a.label().max(b.label()));
                    if clean_before.contains(&key) {
                        assert!(
                            !overlaps(ra, rb),
                            "resize reflow created an overlap between {:?} and {:?}",
                            a,
                            b
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn removing_last_output_keeps_pins_on_placeholder() {
        let mut sim = Sim::new();
        sim.apply(&Op::MapNew {
            x: 0,
            y: 0,
            w: 100,
            h: 100,
        });
        sim.apply(&Op::TogglePin { idx: 0, output: 0 });
        assert_eq!(sim.stage.pin_of(&sim.windows[0]).unwrap().output, "OUT-0");

        // Drop the two other outputs (non-last), then OUT-0 (last).
        sim.apply(&Op::RemoveOutput { output: 2 });
        sim.apply(&Op::RemoveOutput { output: 1 });
        assert_eq!(sim.live, ["OUT-0".to_string()]);
        assert_eq!(sim.stage.pin_of(&sim.windows[0]).unwrap().output, "OUT-0");

        sim.apply(&Op::RemoveOutput { output: 0 });
        assert!(sim.live.is_empty());
        assert_eq!(sim.placeholder.as_deref(), Some("OUT-0"));
        // The last-output placeholder keeps its pins verbatim.
        assert_eq!(sim.stage.pin_of(&sim.windows[0]).unwrap().output, "OUT-0");
        sim.verify_outputs();
    }

    #[test]
    fn removing_non_last_output_rebinds_pin_and_exits_fullscreen() {
        let mut sim = Sim::new();
        sim.apply(&Op::MapNew {
            x: 0,
            y: 0,
            w: 100,
            h: 100,
        });
        sim.apply(&Op::MapNew {
            x: 500,
            y: 0,
            w: 100,
            h: 100,
        });
        sim.apply(&Op::TogglePin { idx: 0, output: 2 });
        assert_eq!(sim.stage.pin_of(&sim.windows[0]).unwrap().output, "OUT-2");
        sim.apply(&Op::EnterFullscreen { idx: 1, output: 2 });
        assert!(sim.stage.fullscreen_on("OUT-2").is_some());

        sim.apply(&Op::RemoveOutput { output: 2 });
        assert!(!sim.live.iter().any(|o| o == "OUT-2"));
        assert!(sim.stage.fullscreen_on("OUT-2").is_none());
        assert!(!sim.stage.is_fullscreen(&sim.windows[1]));
        // The orphaned pin follows to the first survivor.
        assert_eq!(sim.stage.pin_of(&sim.windows[0]).unwrap().output, "OUT-0");
        sim.verify_outputs();
    }

    #[test]
    fn reconnect_rebinds_orphaned_pins_and_clears_placeholder_fullscreen() {
        let mut sim = Sim::new();
        sim.apply(&Op::MapNew {
            x: 0,
            y: 0,
            w: 100,
            h: 100,
        });
        sim.apply(&Op::MapNew {
            x: 500,
            y: 0,
            w: 100,
            h: 100,
        });
        sim.apply(&Op::TogglePin { idx: 0, output: 0 });
        sim.apply(&Op::RemoveOutput { output: 2 });
        sim.apply(&Op::RemoveOutput { output: 1 });
        sim.apply(&Op::RemoveOutput { output: 0 });
        assert_eq!(sim.placeholder.as_deref(), Some("OUT-0"));
        assert_eq!(sim.stage.pin_of(&sim.windows[0]).unwrap().output, "OUT-0");

        // Fullscreen on the placeholder is legal (a normal space output).
        sim.apply(&Op::EnterFullscreen { idx: 1, output: 0 });
        assert!(sim.stage.fullscreen_on("OUT-0").is_some());

        // Reconnecting a differently-named output swaps out the placeholder:
        // its fullscreen exits and the orphaned pin moves to the new output.
        sim.apply(&Op::AddOutput { output: 1 });
        assert_eq!(sim.live, ["OUT-1".to_string()]);
        assert!(sim.placeholder.is_none());
        assert!(sim.stage.fullscreen_on("OUT-0").is_none());
        assert_eq!(sim.stage.pin_of(&sim.windows[0]).unwrap().output, "OUT-1");
        sim.verify_outputs();
    }

    proptest! {
        #[test]
        fn random_op_sequences_preserve_invariants(
            ops in proptest::collection::vec(op_strategy(), 1..80)
        ) {
            let mut sim = Sim::new();
            for op in &ops {
                sim.apply(op);
                sim.stage.verify_invariants();
                sim.verify_outputs();
                sim.verify_suspended();
            }
        }

        #[test]
        fn fit_fullscreen_round_trips_restore_saved_sizes(
            ops in proptest::collection::vec(op_strategy(), 1..80)
        ) {
            // Exercise the highest-leverage regression site (pre-fit-restore)
            // through full random sequences, then unwind every window still
            // fit or fullscreen and check the recorded configures.
            let mut sim = Sim::new();
            for op in &ops {
                sim.apply(op);
            }
            for output in OUTPUTS {
                sim.exit_fullscreen(output);
            }
            let windows: Vec<TestWindow> = sim.stage.windows().cloned().collect();
            for w in windows {
                if sim.stage.is_fit(&w) {
                    let saved = sim.stage.take_fit_saved_size(&w).unwrap();
                    prop_assert_eq!(Some(saved), sim.fit_expect.remove(&w.label()));
                    w.exit_fit_configure(saved);
                }
            }
            // Every exit configure carried a non-empty size.
            for w in &sim.windows {
                for c in w.sent_configures() {
                    if let SentConfigure::ExitFullscreen(s) | SentConfigure::ExitFit(s) = c {
                        prop_assert!(s.w > 0 && s.h > 0, "restore configure with empty size");
                    }
                }
            }
            sim.stage.verify_invariants();
            sim.verify_outputs();
            sim.verify_suspended();
        }
    }
}
