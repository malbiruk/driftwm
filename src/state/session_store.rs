//! Compositor-side glue for the durable session store (session restore): build
//! an envelope from live state, write it through the [`driftwm::session`] IO,
//! and materialize it back into suspended windows at startup.
//!
//! Cadence: a create or dismiss writes immediately; a move or resize arms a
//! short debounce timer; graceful shutdown fsync's a final write. Suspended
//! windows are saved regardless of `restore_windows`; live windows are saved
//! as `Quit` records only when it's on. `path == None` disables everything (a
//! winit dev session without `--session-file`, or a fixture without an
//! injected path).

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use smithay::desktop::Window;
use smithay::reexports::calloop::RegistrationToken;
use smithay::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay::utils::{Logical, Point, Rectangle, Size};
use smithay::wayland::seat::WaylandFocus;

use driftwm::canvas::{ScreenPos, internal_to_rule, rule_to_internal, screen_to_canvas};
use driftwm::desktop_entry::AppIdentity;
use driftwm::session::{self, Origin, SessionEntry, SessionEnvelope, SessionOutput};
use driftwm::window_ext::WindowExt;

use super::{DriftWm, StageWindow, SuspendedId, SuspendedWindow, output_state};

/// How long a move/resize coalesces before the durable write lands.
const WRITE_DEBOUNCE: Duration = Duration::from_secs(1);

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
        let (materialize, carried) =
            session::partition_for_restore(entries, self.config.session.restore_windows);
        self.session_store.carried_forward = carried;
        for entry in materialize {
            self.materialize_entry(entry);
        }
    }

    /// Recreate one saved window as a dormant suspended stand-in at its canvas
    /// rect. A fresh per-process id is assigned — the durable record key is not
    /// reused across restarts, and nothing in this pass depends on it.
    /// `map_window` raises, so materializing bottom→top reproduces the z-order.
    fn materialize_entry(&mut self, entry: SessionEntry) {
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
            entry.title,
            entry.origin,
            entry.csd,
        ));
        self.map_window(StageWindow::Suspended(s), loc, false);
    }

    /// Per-output cameras to restore on connect: the durable fresh-boot seed
    /// with the runtime state file layered on top, so runtime wins within a
    /// login session and durable only fills gaps the runtime file lacks.
    pub fn saved_camera_state(&self) -> HashMap<String, (Point<f64, Logical>, f64)> {
        // Camera restore is opt-in: without it, a connecting output starts at
        // its default centered camera, so the durable seed is withheld here (it
        // still carries forward on the write side). The runtime state file is
        // unconditional — it drives within-session output reconnects, a
        // separate concern from restoring across restarts.
        let durable = if self.config.session.restore_camera {
            self.session_store.durable_cameras.clone()
        } else {
            HashMap::new()
        };
        merge_saved_cameras(&durable, super::read_all_per_output_state())
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
    /// SIGTERM/SIGHUP), fsync'd. Suspended windows are always saved; live
    /// windows are added as `Quit` records only when `restore_windows` is on.
    pub fn serialize_session_on_shutdown(&mut self) {
        if self.session_store.path.is_none() {
            return;
        }
        self.write_session(self.config.session.restore_windows, true);
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
    /// origin; live windows are appended as `Quit` records when `include_live`.
    /// Carried-forward entries lead so freshly-active windows restore on top.
    fn build_session_envelope(&mut self, include_live: bool) -> SessionEnvelope {
        // The record id is informational (materialization assigns fresh
        // in-process ids); numbering live windows past the suspended ids just
        // keeps them distinct within this write.
        let mut next_live_id = self.next_suspended_id;
        let windows: Vec<StageWindow> = self.stage.windows().cloned().collect();
        // Z-ordered tail: suspended stand-ins + (with restore on) live windows.
        // Tally live windows per app so carried quit records can be deduped
        // against the apps that actually came back.
        let mut tail: Vec<SessionEntry> = Vec::new();
        let mut live_counts: HashMap<String, usize> = HashMap::new();
        for window in &windows {
            if let Some(s) = window.suspended() {
                let loc = self.stage.position_of(window).unwrap_or_default();
                tail.push(suspended_entry(s, loc));
            } else if include_live
                && let Some(entry) = self.live_window_entry(window, &mut next_live_id)
            {
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
            entries.push(carried.clone());
        }
        entries.extend(tail);

        SessionEnvelope {
            version: session::VERSION,
            saved_at: now_unix(),
            entries,
            outputs: self.per_output_cameras(),
        }
    }

    /// A `Quit` record for one live client window, or `None` when it can't come
    /// back: a widget, a dialog (has a parent — dead or alive — or is modal,
    /// matching suspend eligibility), or an app that resolves to no `.desktop`
    /// entry.
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
        let identity = self.resolve_identity(&app_id)?;
        let title = window.window_title().unwrap_or_default();
        // A CSD window (no SSD bar) restores to a stand-in whose body is shrunk
        // under the bar, so persist that shrunken body — the same rect a live
        // suspend leaves, so restore + adopt reproduce the original footprint.
        let csd = self.window_ssd_bar(window) == 0;
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
            title,
            position: [x, y],
            size: [body.size.w, body.size.h],
            origin: Origin::Quit,
            csd,
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
        title: s.last_title.clone(),
        position: [x, y],
        size: [size.w, size.h],
        origin: s.origin,
        csd: s.csd,
    }
}

/// Merge the durable fresh-boot seed under the runtime file, which wins.
fn merge_saved_cameras(
    durable: &HashMap<String, (Point<f64, Logical>, f64)>,
    runtime: HashMap<String, (Point<f64, Logical>, f64)>,
) -> HashMap<String, (Point<f64, Logical>, f64)> {
    let mut merged = durable.clone();
    merged.extend(runtime);
    merged
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
            title: "t".into(),
            position,
            size,
            origin: Origin::Explicit,
            csd: false,
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
        durable.insert("only-durable".to_string(), (Point::from((1.0, 2.0)), 1.0));
        durable.insert("shared".to_string(), (Point::from((3.0, 4.0)), 1.5));

        let mut runtime = HashMap::new();
        runtime.insert("shared".to_string(), (Point::from((9.0, 9.0)), 2.0));
        runtime.insert("only-runtime".to_string(), (Point::from((5.0, 6.0)), 0.5));

        let merged = merge_saved_cameras(&durable, runtime);
        // A durable-only output is seeded on fresh boot.
        assert_eq!(merged["only-durable"], (Point::from((1.0, 2.0)), 1.0));
        // The runtime file wins within a login session.
        assert_eq!(merged["shared"], (Point::from((9.0, 9.0)), 2.0));
        assert_eq!(merged["only-runtime"], (Point::from((5.0, 6.0)), 0.5));
    }
}
