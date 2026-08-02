//! Compositor-side glue for the durable session store (session restore): build
//! an envelope from live state, write it through the [`driftwm::session`] IO,
//! and materialize it back into suspended windows at startup.
//!
//! Cadence: a create or dismiss writes immediately; a move or resize arms a
//! short debounce timer; graceful shutdown fsync's a final write. Suspended
//! windows are saved regardless of `restore_windows`; a live window is saved as
//! a `Quit` record when that flag resolves on for it (global default or a
//! per-app rule). `path == None` disables everything (a
//! winit dev session without `--session-file`, or a fixture without an
//! injected path).

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use smithay::desktop::Window;
use smithay::reexports::calloop::RegistrationToken;
use smithay::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay::utils::{Logical, Point, Rectangle, SERIAL_COUNTER, Size};
use smithay::wayland::seat::WaylandFocus;

use driftwm::canvas::{ScreenPos, internal_to_rule, rule_to_internal, screen_to_canvas};
use driftwm::desktop_entry::AppIdentity;
use driftwm::session::{self, Origin, SessionEntry, SessionEnvelope, SessionOutput};
use driftwm::window_ext::WindowExt;

use super::{
    AUTO_PLACE_CLUSTER_THRESHOLD, DriftWm, StageWindow, SuspendedId, SuspendedWindow, output_state,
};

/// How long a move/resize coalesces before the durable write lands.
const WRITE_DEBOUNCE: Duration = Duration::from_secs(1);

/// A per-output viewport seed waiting for its output to connect. The two
/// sources speak different conventions, so the variant carries which one this
/// is instead of leaving two meanings under one `Point`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum CameraSeed {
    /// The durable envelope's internal top-left camera, Y-down.
    Camera(Point<f64, Logical>),
    /// The runtime state file's published viewport centre, Y-up. Resolving it
    /// needs the output's logical size and zoom, so it stays unresolved until
    /// [`DriftWm::output_connected`].
    Center { x: f64, y: f64 },
}

impl CameraSeed {
    /// The internal camera this seed stands for on an output of `logical` size
    /// at `zoom`.
    pub fn resolve(self, zoom: f64, logical: Size<i32, Logical>) -> Point<f64, Logical> {
        match self {
            CameraSeed::Camera(camera) => camera,
            CameraSeed::Center { x, y } => driftwm::canvas::camera_for_center(x, y, zoom, logical),
        }
    }
}

/// Runtime bookkeeping for the durable session store.
#[derive(Default)]
pub struct SessionStore {
    /// Durable file path. `None` disables all persistence.
    pub path: Option<PathBuf>,
    /// `Quit`-origin entries read at startup but not materialized (restore
    /// off), re-emitted on every write so a flag-off session never destroys
    /// the saved session.
    pub(crate) carried_forward: Vec<SessionEntry>,
    /// Per-output cameras read at startup, to seed outputs the runtime state
    /// file hasn't recorded yet (fresh boot).
    pub(crate) durable_cameras: HashMap<String, (Point<f64, Logical>, f64)>,
    /// The bookmark registry read from the file at startup, stashed
    /// unconditionally (mirrors `durable_cameras`).
    pub(crate) durable_bookmarks: BTreeMap<String, [f64; 2]>,
    /// The stand-in that materialized from the focused entry, waiting for
    /// `apply_restored_focus` to hand it the focus once the boot outputs exist.
    /// Survives a refused hand-over: the write side re-emits the flag for it
    /// while nothing else holds focus, so an off-screen boot defers the record
    /// to the next one instead of erasing it.
    pub(crate) restore_focus: Option<SuspendedId>,
    /// A change is waiting for the debounce timer to write it.
    dirty: bool,
    /// The armed one-shot debounce timer, if any.
    timer: Option<RegistrationToken>,
}

impl DriftWm {
    /// Read the durable session at startup: stash per-output cameras for
    /// fresh-boot seeding, materialize the eligible entries as suspended
    /// windows (bottom→top), and hold the rest to carry forward.
    pub fn load_session(&mut self) {
        let Some(path) = self.session_store.path.clone() else {
            return;
        };
        let envelope = session::read(&path);
        // Always stash the durable cameras, even with restore off: the write
        // side carries them forward for outputs not currently connected (see
        // `per_output_cameras`), so an unplugged monitor's viewport survives the
        // next steady-state rewrite. The seed is only *applied* to a connecting
        // output when `restore_camera` is on (see `saved_camera_state`), so
        // flipping the flag on later restores from a file that never lost it.
        self.session_store.durable_cameras = envelope
            .outputs
            .iter()
            .filter(|(_, o)| valid_camera_seed(Point::from((o.camera[0], o.camera[1])), o.zoom))
            .map(|(name, o)| {
                (
                    name.clone(),
                    (Point::from((o.camera[0], o.camera[1])), o.zoom),
                )
            })
            .collect();
        // Drop entries with out-of-range geometry entirely — neither
        // materialized nor carried forward, so a hand-edit or a flipped byte
        // that would panic `Size::from` (debug) or overflow `rule_to_internal`
        // self-heals on the next write instead of crashing every startup.
        let entries: Vec<SessionEntry> = envelope
            .entries
            .into_iter()
            .filter(valid_entry_geometry)
            .collect();
        let (materialize, carried) = session::partition_for_restore(entries, |e| {
            self.resolve_restore_windows_for_record(&e.app_id)
        });
        self.session_store.carried_forward = carried;
        for entry in materialize {
            let held_focus = entry.focused;
            let sid = self.materialize_entry(entry);
            // Last flag wins: a hand-edited file with several entries flagged
            // resolves to the last one, not the first. This build's own writes
            // never produce more than one.
            if held_focus {
                self.session_store.restore_focus = Some(sid);
            }
        }
        // Stash the file's registry unconditionally: with the flag off the write
        // side carries it forward verbatim (so flipping the flag on later loses
        // nothing), and with it on we overlay it onto the config seeds here — a
        // restored value wins per name, config seeds fill the names the file lacks.
        self.session_store.durable_bookmarks = envelope.bookmarks;
        if self.config.session.restore_bookmarks {
            let overlay = self.session_store.durable_bookmarks.clone();
            for (name, value) in overlay {
                self.bookmarks.insert(name, value);
            }
        }
    }

    /// Recreate one saved window as a dormant suspended stand-in at its canvas
    /// rect. A fresh per-process id is assigned — the durable record key is not
    /// reused across restarts, and nothing in this pass depends on it.
    /// `map_window` raises, so materializing bottom→top reproduces the z-order.
    fn materialize_entry(&mut self, entry: SessionEntry) -> SuspendedId {
        let size = Size::from((entry.size[0], entry.size[1]));
        let loc = rule_to_internal(entry.position[0], entry.position[1], size);
        let sid = SuspendedId(self.next_suspended_id);
        self.next_suspended_id += 1;
        let identity = AppIdentity {
            app_id: entry.app_id,
            desktop_id: entry.desktop_id,
            display_name: entry.display_name,
        };
        let s = Rc::new(SuspendedWindow::new(
            sid,
            size,
            identity,
            entry.origin,
            entry.csd,
        ));
        self.map_window(StageWindow::Suspended(s), loc, false);
        sid
    }

    /// Hand the focus back to the stand-in the session recorded as focused, so a
    /// new window's auto placement anchors to the cluster the user left off on.
    /// Runs once the boot outputs and their seeded cameras exist, and only when
    /// enough of the stand-in shows on *some* output that the user can see what
    /// they'd be relaunching or dismissing. The fraction is auto placement's,
    /// but the scope is not: auto placement weighs an anchor against the active
    /// output alone, so a stand-in restored onto another monitor holds the focus
    /// yet still won't anchor a window opened on the active one.
    ///
    /// A refused hand-over leaves the record pending rather than dropping it, so
    /// a boot that can't use it hands it to the next one (see
    /// `build_session_envelope`).
    pub fn apply_restored_focus(&mut self) {
        let Some(id) = self.session_store.restore_focus else {
            return;
        };
        let Some(standin) = self.find_suspended(id) else {
            self.session_store.restore_focus = None;
            return;
        };
        let app_id = standin.identity.app_id.clone();
        let element = StageWindow::Suspended(standin);
        let visible = self.space.outputs().any(|output| {
            self.window_visible_at_least_on(&element, output, AUTO_PLACE_CLUSTER_THRESHOLD)
        });
        if !visible {
            tracing::debug!("restored focus for {app_id} deferred: its stand-in is off screen");
            return;
        }
        self.session_store.restore_focus = None;
        self.set_suspended_focus(id, SERIAL_COUNTER.next_serial());
    }

    /// Per-output cameras to restore on connect: the durable fresh-boot seed
    /// with the runtime state file layered on top, so runtime wins within a
    /// login session and durable only fills gaps the runtime file lacks.
    pub fn saved_camera_state(&self) -> HashMap<String, (CameraSeed, f64)> {
        // Camera restore is opt-in: without it, a connecting output starts at
        // its default centered camera, so the durable seed is withheld here (it
        // still carries forward on the write side). The runtime state file is
        // unconditional — it drives within-session output reconnects, a
        // separate concern from restoring across restarts.
        let durable = if self.config.session.restore_camera {
            self.session_store
                .durable_cameras
                .iter()
                .map(|(name, &(cam, zoom))| (name.clone(), (CameraSeed::Camera(cam), zoom)))
                .collect()
        } else {
            HashMap::new()
        };
        merge_saved_cameras(durable, super::read_all_per_output_state())
    }

    /// Immediate write for a create/dismiss: cancel any pending debounce and
    /// flush now, so a user-visible change is durable at once.
    pub fn session_store_write_now(&mut self) {
        if self.session_store.path.is_none() {
            return;
        }
        if let Some(token) = self.session_store.timer.take() {
            self.loop_handle.remove(token);
        }
        self.session_store_flush();
    }

    /// Whether a change is waiting on the debounce timer. Both write paths write
    /// unconditionally, so an armed debounce has no other seam to observe short
    /// of waiting out the wall-clock second.
    #[cfg(test)]
    pub(crate) fn session_store_dirty(&self) -> bool {
        self.session_store.dirty
    }

    /// Arm the debounced write for a move/resize: a one-shot ~1s timer coalesces
    /// a drag's stream of position/size updates into a single write.
    pub fn session_store_mark_dirty(&mut self) {
        if self.session_store.path.is_none() {
            return;
        }
        self.session_store.dirty = true;
        if self.session_store.timer.is_some() {
            return;
        }
        let timer = Timer::from_duration(WRITE_DEBOUNCE);
        self.session_store.timer = self
            .loop_handle
            .insert_source(timer, |_, _, data: &mut DriftWm| {
                data.session_store.timer = None;
                if data.session_store.dirty {
                    data.session_store_flush();
                }
                TimeoutAction::Drop
            })
            .ok();
    }

    /// Flush the durable session at graceful shutdown (keybind quit or
    /// SIGTERM/SIGHUP), fsync'd. Suspended windows are always saved; a live
    /// window is added as a `Quit` record when `restore_windows` resolves on for
    /// it — per-window, not a global gate, so a rule can opt an app in while the
    /// section key is off, or out while it's on.
    pub fn serialize_session_on_shutdown(&mut self) {
        if self.session_store.path.is_none() {
            return;
        }
        self.write_session(true, true);
    }

    /// Steady-state write: suspended windows + carried-forward + cameras, no
    /// live windows, no fsync. Clears the dirty flag.
    fn session_store_flush(&mut self) {
        self.session_store.dirty = false;
        self.write_session(false, false);
    }

    fn write_session(&mut self, include_live: bool, fsync: bool) {
        let Some(path) = self.session_store.path.clone() else {
            return;
        };
        let envelope = self.build_session_envelope(include_live);
        if let Err(err) = session::write(&path, &envelope, fsync) {
            tracing::warn!("failed to write durable session store: {err}");
        }
    }

    /// Serialize the current durable state. Suspended windows carry their own
    /// origin; live windows are appended as `Quit` records when `include_live`
    /// and their resolved `restore_windows` allows it.
    /// Carried-forward entries lead so freshly-active windows restore on top.
    fn build_session_envelope(&mut self, include_live: bool) -> SessionEnvelope {
        // The record id is informational (materialization assigns fresh
        // in-process ids); numbering live windows past the suspended ids just
        // keeps them distinct within this write.
        let mut next_live_id = self.next_suspended_id;
        let windows: Vec<StageWindow> = self.stage.windows().cloned().collect();
        // Focus *intent*, the same anchor auto placement reads, so a launcher's
        // transient keyboard focus at shutdown doesn't erase the real one.
        let focused = self.focused_anchor_element();
        // A hand-over this boot refused keeps its flag while nobody else holds
        // focus, so the ordinary `restore_camera = false` boot — stand-in lands
        // off screen, grant declined — doesn't erase the record on its first
        // write. A real focus supersedes and retires it.
        if focused.is_some() {
            self.session_store.restore_focus = None;
        }
        let pending_focus = self.session_store.restore_focus;
        // Z-ordered tail: suspended stand-ins + (with restore on) live windows.
        // Tally live windows per app so carried quit records can be deduped
        // against the apps that actually came back.
        let mut tail: Vec<SessionEntry> = Vec::new();
        let mut live_counts: HashMap<String, usize> = HashMap::new();
        for window in &windows {
            let has_focus = focused.as_ref() == Some(window);
            if let Some(s) = window.suspended() {
                let loc = self.stage.position_of(window).unwrap_or_default();
                let mut entry = suspended_entry(s, loc);
                // `pending_focus` is `None` whenever anything holds focus, so
                // the two sources can never flag two entries.
                entry.focused = has_focus || pending_focus == Some(s.id);
                tail.push(entry);
            } else if include_live
                && let Some(mut entry) = self.live_window_entry(window, &mut next_live_id)
            {
                entry.focused = has_focus;
                *live_counts.entry(entry.app_id.clone()).or_default() += 1;
                tail.push(entry);
            }
        }

        // With restore on, a relaunched app is serialized live above, so drop one
        // carried quit record per live window of the same app (count-matched) —
        // unmatched carries, and every explicit carry, survive to the next boot.
        let mut entries: Vec<SessionEntry> = Vec::new();
        for carried in &self.session_store.carried_forward {
            if include_live
                && carried.origin == Origin::Quit
                && let Some(remaining) = live_counts.get_mut(&carried.app_id)
                && *remaining > 0
            {
                *remaining -= 1;
                continue;
            }
            let mut entry = carried.clone();
            // The focus of a boot that's over: clearing it keeps exactly one
            // flagged entry in the file, the one focused at this write.
            entry.focused = false;
            entries.push(entry);
        }
        entries.extend(tail);

        SessionEnvelope {
            version: session::VERSION,
            saved_at: now_unix(),
            entries,
            outputs: self.per_output_cameras(),
            // With restore on, the live registry is the durable one. With it off,
            // runtime edits are ephemeral, so carry the stashed file registry
            // forward untouched instead of overwriting it with the seeds.
            bookmarks: if self.config.session.restore_bookmarks {
                self.bookmarks.clone()
            } else {
                self.session_store.durable_bookmarks.clone()
            },
        }
    }

    /// The effective `restore_windows` for `(app_id, title)`: a matching window
    /// rule's override wins, else the global default. Resolved live (not the
    /// stamped applied rule) so a hot-reload takes effect on the next shutdown.
    fn resolve_restore_windows(&self, app_id: &str, title: &str) -> bool {
        self.config
            .resolve_window_rules(app_id, title)
            .and_then(|r| r.restore_windows)
            .unwrap_or(self.config.session.restore_windows)
    }

    /// The effective `restore_windows` for a saved record. A record carries an
    /// `app_id` but no title, so a rule is keyed on its `app_id` predicate
    /// alone: one that also matches on `title` narrows what gets *saved*, but
    /// answers for every record of that app on the way back. Resolving it
    /// against an empty title instead would miss, and the record would come back
    /// as a stand-in that saves itself again every cycle. A rule with no
    /// `app_id` can't be keyed to a record and is skipped, rather than left to
    /// answer for every app. Last match wins, as in `resolve_window_rules`.
    fn resolve_restore_windows_for_record(&self, app_id: &str) -> bool {
        self.config
            .window_rules
            .iter()
            .rev()
            .filter(|rule| rule.app_id.as_ref().is_some_and(|p| p.matches(app_id)))
            .find_map(|rule| rule.restore_windows)
            .unwrap_or(self.config.session.restore_windows)
    }

    /// A `Quit` record for one live client window, or `None` when it can't come
    /// back: a widget, a dialog (has a parent — dead or alive — or is modal,
    /// matching suspend eligibility), an app a `restore_windows = false` rule
    /// keeps out of the save, or an app that resolves to no `.desktop` entry.
    fn live_window_entry(
        &mut self,
        window: &StageWindow,
        next_id: &mut u64,
    ) -> Option<SessionEntry> {
        let client = window.client()?.clone();
        if window.is_widget() || window.parent_surface().is_some() || window.is_modal() {
            return None;
        }
        let app_id = window.app_id_or_class().unwrap_or_default();
        let title = window.window_title().unwrap_or_default();
        if !self.resolve_restore_windows(&app_id, &title) {
            return None;
        }
        let identity = self.resolve_identity(&app_id)?;
        // A CSD window restores to a stand-in whose body is shrunk under the
        // bar, so persist that shrunken body — the same rect a live suspend
        // leaves, so restore + adopt reproduce the original footprint.
        let csd = client.wl_surface().is_none_or(|s| self.surface_is_csd(&s));
        let (loc, size) = self.live_window_rect(&client);
        let body = self.standin_body_rect(Rectangle::new(loc, size), csd);
        let (x, y) = internal_to_rule(body.loc, body.size);
        let id = *next_id;
        *next_id += 1;
        Some(SessionEntry {
            id,
            app_id: identity.app_id,
            desktop_id: identity.desktop_id,
            display_name: identity.display_name,
            position: [x, y],
            size: [body.size.w, body.size.h],
            origin: Origin::Quit,
            csd,
            focused: false,
        })
    }

    /// The canvas rect a live window restores to. Fullscreen and pinned windows
    /// live in screen space, so use the geometry the stand-in would land at: the
    /// pre-fullscreen saved rect, or the unpin-to-canvas landing.
    fn live_window_rect(&self, window: &Window) -> (Point<i32, Logical>, Size<i32, Logical>) {
        if let Some(output) = window
            .wl_surface()
            .and_then(|s| self.find_fullscreen_output_for_surface(&s))
            && let Some(entry) = self.stage.fullscreen_on(&output.name())
        {
            return (entry.saved_location, entry.saved_size);
        }
        if let Some(site) = self.stage.pin_of(window).cloned()
            && let Some(output) = self.output_by_name(&site.output)
        {
            let (camera, zoom) = {
                let os = output_state(&output);
                (os.camera, os.zoom)
            };
            let canvas = screen_to_canvas(ScreenPos(site.screen_pos.to_f64()), camera, zoom)
                .0
                .to_i32_round();
            return (canvas, window.geometry().size);
        }
        let loc = self.stage.position_of(window).unwrap_or_default();
        (loc, window.geometry().size)
    }

    /// Current per-output cameras, plus stale entries for outputs that were
    /// present at boot but are gone now (an unplugged monitor's viewport isn't
    /// lost — matching the runtime file's behavior).
    fn per_output_cameras(&self) -> BTreeMap<String, SessionOutput> {
        let mut outputs = BTreeMap::new();
        for output in self.space.outputs() {
            let os = output_state(output);
            outputs.insert(
                output.name(),
                SessionOutput {
                    camera: [os.camera.x, os.camera.y],
                    zoom: os.zoom,
                },
            );
        }
        for (name, (cam, zoom)) in &self.session_store.durable_cameras {
            outputs.entry(name.clone()).or_insert(SessionOutput {
                camera: [cam.x, cam.y],
                zoom: *zoom,
            });
        }
        outputs
    }
}

/// A durable entry for a suspended window at canvas position `loc`.
fn suspended_entry(s: &SuspendedWindow, loc: Point<i32, Logical>) -> SessionEntry {
    let size = s.size.get();
    let (x, y) = internal_to_rule(loc, size);
    SessionEntry {
        id: s.id.0,
        app_id: s.identity.app_id.clone(),
        desktop_id: s.identity.desktop_id.clone(),
        display_name: s.identity.display_name.clone(),
        position: [x, y],
        size: [size.w, size.h],
        origin: s.origin,
        csd: s.csd,
        focused: false,
    }
}

/// Merge the durable fresh-boot seed under the runtime file, which wins.
fn merge_saved_cameras(
    mut durable: HashMap<String, (CameraSeed, f64)>,
    runtime: HashMap<String, (CameraSeed, f64)>,
) -> HashMap<String, (CameraSeed, f64)> {
    durable.extend(runtime);
    durable
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Whether a saved window's geometry is safe to feed the stage: size components
/// in `1..=32767` (smithay's `Size::from` debug-asserts non-negative; the upper
/// bound keeps render buffers sane) and positions within a range that can't
/// overflow `rule_to_internal`'s `i32` math.
fn valid_entry_geometry(entry: &SessionEntry) -> bool {
    const POSITION_LIMIT: i32 = 16_000_000;
    let [w, h] = entry.size;
    let [x, y] = entry.position;
    let ok = (1..=32767).contains(&w)
        && (1..=32767).contains(&h)
        && (-POSITION_LIMIT..=POSITION_LIMIT).contains(&x)
        && (-POSITION_LIMIT..=POSITION_LIMIT).contains(&y);
    if !ok {
        tracing::warn!(
            "session store: dropping '{}' with out-of-range geometry (size {w}x{h}, pos {x},{y})",
            entry.app_id
        );
    }
    ok
}

/// Whether a durable/runtime camera seed is safe to apply: finite components
/// within a sane canvas range and a zoom inside the real zoom bounds. An
/// invalid seed (`zoom: 0.0`, non-finite, corruption) is skipped so it can't
/// warp the pointer to infinity or divide every canvas conversion by zero.
pub(crate) fn valid_camera_seed(camera: Point<f64, Logical>, zoom: f64) -> bool {
    const CAMERA_LIMIT: f64 = 1e9;
    camera.x.is_finite()
        && camera.y.is_finite()
        && camera.x.abs() <= CAMERA_LIMIT
        && camera.y.abs() <= CAMERA_LIMIT
        && zoom.is_finite()
        && (driftwm::canvas::MIN_ZOOM_FLOOR..=driftwm::canvas::MAX_ZOOM).contains(&zoom)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn geom_entry(size: [i32; 2], position: [i32; 2]) -> SessionEntry {
        SessionEntry {
            id: 1,
            app_id: "app".into(),
            desktop_id: "app.desktop".into(),
            display_name: "App".into(),
            position,
            size,
            origin: Origin::Explicit,
            csd: false,
            focused: false,
        }
    }

    #[test]
    fn entry_geometry_rejects_out_of_range() {
        assert!(valid_entry_geometry(&geom_entry([400, 300], [100, 200])));
        assert!(
            !valid_entry_geometry(&geom_entry([-1, 300], [0, 0])),
            "negative size rejected (would panic Size::from in debug)"
        );
        assert!(
            !valid_entry_geometry(&geom_entry([0, 300], [0, 0])),
            "zero size"
        );
        assert!(
            !valid_entry_geometry(&geom_entry([40000, 300], [0, 0])),
            "oversize"
        );
        assert!(
            !valid_entry_geometry(&geom_entry([400, 300], [20_000_000, 0])),
            "extreme position that would overflow rule_to_internal"
        );
        assert!(!valid_entry_geometry(&geom_entry(
            [400, 300],
            [0, i32::MIN]
        )));
    }

    #[test]
    fn camera_seed_rejects_bad_zoom_and_nonfinite() {
        let cam = Point::from((-960.0, -540.0));
        assert!(valid_camera_seed(cam, 1.0));
        assert!(!valid_camera_seed(cam, 0.0), "zero zoom breaks canvas math");
        assert!(!valid_camera_seed(cam, -1.0));
        assert!(!valid_camera_seed(cam, f64::INFINITY));
        assert!(!valid_camera_seed(cam, f64::NAN));
        assert!(!valid_camera_seed(cam, 1000.0), "beyond MAX_ZOOM");
        assert!(!valid_camera_seed(Point::from((f64::NAN, 0.0)), 1.0));
        assert!(!valid_camera_seed(Point::from((1e12, 0.0)), 1.0));
    }

    #[test]
    fn runtime_camera_wins_over_durable_seed() {
        let mut durable = HashMap::new();
        durable.insert(
            "only-durable".to_string(),
            (CameraSeed::Camera(Point::from((1.0, 2.0))), 1.0),
        );
        durable.insert(
            "shared".to_string(),
            (CameraSeed::Camera(Point::from((3.0, 4.0))), 1.5),
        );

        let mut runtime = HashMap::new();
        runtime.insert(
            "shared".to_string(),
            (CameraSeed::Center { x: 9.0, y: 9.0 }, 1.0),
        );
        runtime.insert(
            "only-runtime".to_string(),
            (CameraSeed::Center { x: 5.0, y: 6.0 }, 0.5),
        );

        let merged = merge_saved_cameras(durable, runtime);
        // A durable-only output is seeded on fresh boot.
        assert_eq!(
            merged["only-durable"],
            (CameraSeed::Camera(Point::from((1.0, 2.0))), 1.0)
        );
        // The runtime file wins within a login session.
        assert_eq!(
            merged["shared"],
            (CameraSeed::Center { x: 9.0, y: 9.0 }, 1.0)
        );
        assert_eq!(
            merged["only-runtime"],
            (CameraSeed::Center { x: 5.0, y: 6.0 }, 0.5)
        );
    }

    #[test]
    fn resolving_a_center_cannot_smuggle_a_bad_camera_past_validation() {
        let logical = Size::from((1920, 1080));

        // A corrupt zoom divides the half-viewport to infinity. Validating
        // under a sane zoom isolates the camera clauses as what rejects it.
        let camera = CameraSeed::Center { x: 0.0, y: 0.0 }.resolve(0.0, logical);
        assert!(!camera.x.is_finite() && !camera.y.is_finite());
        assert!(
            !valid_camera_seed(camera, 1.0),
            "a non-finite camera is refused even under a sane zoom"
        );

        // A centre inside the canvas limit can still resolve past it.
        let camera = CameraSeed::Center { x: -1e9, y: 0.0 }.resolve(0.5, logical);
        assert!(!valid_camera_seed(camera, 0.5));
    }
}
