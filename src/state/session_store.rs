//! Compositor-side glue for the durable session store (session restore): build
//! an envelope from live state, write it through the [`driftwm::session`] IO,
//! and materialize it back into suspended windows at startup.
//!
//! Cadence: every durable change — a create, dismiss, adopt, settled
//! move/resize, viewport motion, focus change — arms a debounce timer, and that
//! timer's flush is the only writer. Nothing writes at shutdown, deliberately:
//! a logout SIGTERMs the compositor and its clients together, so client
//! teardown dispatches in the same event-loop batch that stops the loop and any
//! rebuild from there serializes a stage that is already draining (see
//! [`DriftWm::session_store_mark_dirty`]). The costs are a tail of up to
//! [`WRITE_DEBOUNCE`] ([`CAMERA_WRITE_DEBOUNCE`] when only the viewport moved),
//! and no fsync — a power cut can lose the last write, while a crash or SIGKILL
//! cannot, since the page cache outlives the process.
//! Suspended windows are saved regardless of `restore_windows`; a live window
//! is saved as a `Quit` record when that flag resolves on for it (global
//! default or a per-app rule). `path == None` disables everything (a
//! winit dev session without `--session-file`, or a fixture without an
//! injected path).

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant};

use smithay::desktop::Window;
use smithay::reexports::calloop::RegistrationToken;
use smithay::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay::utils::{Logical, Point, Rectangle, SERIAL_COUNTER, Size};
use smithay::wayland::seat::WaylandFocus;

use driftwm::canvas::{Chrome, ScreenPos, content_to_rule, rule_to_internal, screen_to_canvas};
use driftwm::desktop_entry::AppIdentity;
use driftwm::session::{self, Origin, SessionEntry, SessionEnvelope, SessionOutput};
use driftwm::window_ext::WindowExt;

use super::persistence::viewport_moved;
use super::{
    AUTO_PLACE_CLUSTER_THRESHOLD, DriftWm, StageWindow, SuspendedId, SuspendedWindow, output_state,
};

/// How long a move/resize coalesces before the durable write lands.
pub(crate) const WRITE_DEBOUNCE: Duration = Duration::from_secs(1);

/// The same for viewport motion, which is far cheaper to lose: a camera is
/// continuous and self-correcting — wherever a pan ends is what the next flush
/// records — and `restore_camera` is off by default, so most sessions never read
/// the saved one back. A window mutation is a discrete event the user expects to
/// survive, so it keeps the short interval. Panning being the primary
/// interaction on this canvas, sharing that interval would rewrite the file once
/// a second for the length of every gesture.
const CAMERA_WRITE_DEBOUNCE: Duration = Duration::from_secs(5);

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
    /// Per-output camera/zoom as of the last viewport motion that armed the
    /// debounce — the baseline [`DriftWm::session_store_watch_cameras`] diffs
    /// against. Keyed by output name, and dropped there on the first iteration
    /// that finds the output gone. Bounded by the connected outputs, so unlike
    /// its sibling `state_file_cameras` it is deliberately absent from
    /// `debug_counters`: the fixture does drive this one, and every scenario
    /// with a session path would end above baseline.
    last_seen_cameras: HashMap<String, (Point<f64, Logical>, f64)>,
    /// A change is waiting for the debounce timer to write it.
    dirty: bool,
    /// The armed one-shot debounce timer, if any.
    timer: Option<RegistrationToken>,
    /// When that timer comes due, so a later change can tell whether the armed
    /// flush already covers it. Set and cleared with `timer` — a deadline left
    /// behind after the token is gone would answer for a timer that no longer
    /// exists.
    deadline: Option<Instant>,
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
        // Validated before the migration below, in the convention the numbers on
        // disk are actually in.
        let chrome = self.suspended_chrome();
        let entries: Vec<SessionEntry> = envelope
            .entries
            .into_iter()
            .filter(valid_entry_geometry)
            .map(|entry| {
                if envelope.version == 1 {
                    body_entry_to_frame(entry, chrome)
                } else {
                    entry
                }
            })
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
        // The record is a visual frame; the stand-in stores the body inside it.
        // Positioned from the record's own frame size rather than by re-inflating
        // the body: a frame smaller than its chrome floors to a 1px body, and
        // re-inflating that would land the top-left somewhere else.
        let chrome = self.suspended_chrome();
        let frame = Size::from((entry.size[0], entry.size[1]));
        let size = chrome.content_size(frame);
        let loc = chrome.content_loc(rule_to_internal(
            entry.position[0],
            entry.position[1],
            frame,
        ));
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

    /// Cancel a pending debounce and flush now. Test-only on purpose: a
    /// synchronous rebuild is exactly what [`DriftWm::session_store_mark_dirty`]
    /// exists to avoid, so gating this keeps production unable to name a
    /// synchronous writer at all. The fixture needs one because the debounce is
    /// a real calloop timer with no injectable clock.
    #[cfg(test)]
    pub fn session_store_write_now(&mut self) {
        if self.session_store.path.is_none() {
            return;
        }
        self.session_store_cancel_debounce();
        self.session_store_flush();
    }

    /// Drop a pending debounced write without running it. The signal handler
    /// does this before stopping the loop: an expired timer is dispatched after
    /// the batch's fd events, so a debounce that comes due alongside SIGTERM
    /// would flush from a stage the client disconnects had already drained.
    pub(crate) fn session_store_cancel_debounce(&mut self) {
        self.session_store.deadline = None;
        if let Some(token) = self.session_store.timer.take() {
            self.loop_handle.remove(token);
        }
    }

    /// Whether a change is waiting on the debounce timer. The flush writes
    /// unconditionally, so an armed debounce has no other seam to observe short
    /// of waiting out its wall-clock interval.
    #[cfg(test)]
    pub(crate) fn session_store_dirty(&self) -> bool {
        self.session_store.dirty
    }

    /// Arm the debounced write on the window interval: a one-shot
    /// [`WRITE_DEBOUNCE`] timer coalesces a drag's stream of position/size
    /// updates into a single write. The universal way to queue a durable change,
    /// and the only one production has — viewport motion takes the longer
    /// [`CAMERA_WRITE_DEBOUNCE`] through [`Self::session_store_mark_dirty_after`].
    ///
    /// Nothing may rebuild the envelope synchronously from a handler. calloop
    /// checks the loop's stop flag only after dispatching a whole batch of
    /// ready events, so a logout's client disconnects — which walk
    /// `toplevel_destroyed` → `unmap_window` → `stage.remove` — land in the same
    /// batch as the signal that stops us, in an order nothing controls. A
    /// rebuild reached from any of them writes a half-drained stage over a file
    /// that was correct. Arming instead is safe for the batch carrying the
    /// signal, which cancels the debounce before stopping the loop. One residual
    /// stays: a timer armed in an *earlier* batch still comes due mid-drain (see
    /// [`crate::signals::listen`]) — the same window as a change armed moments
    /// before the logout.
    pub fn session_store_mark_dirty(&mut self) {
        self.session_store_mark_dirty_after(WRITE_DEBOUNCE);
    }

    /// [`Self::session_store_mark_dirty`] with the interval spelled out, so the
    /// camera watcher can queue its change on the longer one. The nearer
    /// deadline wins: an armed flush already due by `delay` covers this change
    /// too, so a camera move can never push a pending window mutation out, while
    /// a window mutation arriving mid-pan pulls the write back in.
    fn session_store_mark_dirty_after(&mut self, delay: Duration) {
        if self.session_store.path.is_none() {
            return;
        }
        self.session_store.dirty = true;
        let deadline = Instant::now() + delay;
        if self.session_store.timer.is_some()
            && self
                .session_store
                .deadline
                .is_some_and(|armed| armed <= deadline)
        {
            return;
        }
        self.session_store_cancel_debounce();
        let timer = Timer::from_duration(delay);
        self.session_store.timer = self
            .loop_handle
            .insert_source(timer, |_, _, data: &mut DriftWm| {
                data.session_store.timer = None;
                data.session_store.deadline = None;
                if data.session_store.dirty {
                    data.session_store_flush();
                }
                TimeoutAction::Drop
            })
            .ok();
        // Derived from the token, not set alongside it: a registration that
        // failed leaves nothing armed, and a deadline standing in for a timer
        // that does not exist would suppress every later arming.
        self.session_store.deadline = self.session_store.timer.is_some().then_some(deadline);
    }

    /// How long the armed debounce still has to run, `None` when nothing is
    /// armed. Test-only: the timer is a real calloop one with no injectable
    /// clock, so scenarios assert which of the two intervals is armed rather
    /// than waiting either out.
    #[cfg(test)]
    pub(crate) fn session_store_debounce_remaining(&self) -> Option<Duration> {
        self.session_store
            .deadline
            .map(|at| at.saturating_duration_since(Instant::now()))
    }

    /// Arm the debounced write when an output's camera or zoom has moved since
    /// the motion that last armed it, on the longer [`CAMERA_WRITE_DEBOUNCE`].
    /// Driven once per event-loop iteration.
    ///
    /// Pan and zoom are the one piece of durable session state nothing else
    /// marks dirty — no `session_store_mark_dirty` site is viewport-driven —
    /// yet the envelope always serializes cameras. Without this, a session where
    /// the user panned and zoomed but touched no window never persists its
    /// viewport.
    ///
    /// The runtime state file's [`DriftWm::write_state_file_if_dirty`] carries a
    /// second camera-delta detector over the same [`viewport_moved`]. They stay
    /// separate because their seed semantics differ: that one arms on an output
    /// it has never cached, since it wants an initial state-file write for it,
    /// while a first sight here must only seed — arming would leave a pending
    /// debounce behind every output connect, boot ones included. That detector
    /// also runs only in the render loops, which no test drives.
    pub fn session_store_watch_cameras(&mut self) {
        if self.session_store.path.is_none() {
            return;
        }
        let live: Vec<(String, Point<f64, Logical>, f64)> = self
            .space
            .outputs()
            .map(|output| {
                let os = output_state(output);
                (output.name(), os.camera, os.zoom)
            })
            .collect();
        // An output that went away drops its baseline instead of arming: a
        // disconnect is not viewport motion, and a replug should re-seed rather
        // than diff against a camera from before the unplug.
        self.session_store
            .last_seen_cameras
            .retain(|name, _| live.iter().any(|(live_name, ..)| live_name == name));

        let mut moved = false;
        for (name, camera, zoom) in live {
            let seen = self.session_store.last_seen_cameras.get(&name).copied();
            if seen.is_some_and(|baseline| !viewport_moved(baseline, (camera, zoom))) {
                // Sub-threshold: the baseline stays where the last arming left
                // it, so a slow continuous pan accumulates into an arming delta
                // instead of creeping past it unrecorded.
                continue;
            }
            moved |= seen.is_some();
            self.session_store
                .last_seen_cameras
                .insert(name, (camera, zoom));
        }
        if moved {
            self.session_store_mark_dirty_after(CAMERA_WRITE_DEBOUNCE);
        }
    }

    /// The debounce's write: live windows (when `restore_windows` allows),
    /// suspended windows, carried-forward entries and cameras. Suspended windows
    /// are always saved; a live window is added as a `Quit` record when
    /// `restore_windows` resolves on for it — per-window, not a global gate, so
    /// a rule can opt an app in while the section key is off, or out while it's
    /// on. Clears the dirty flag.
    fn session_store_flush(&mut self) {
        self.session_store.dirty = false;
        self.write_session(true);
    }

    fn write_session(&mut self, include_live: bool) {
        let Some(path) = self.session_store.path.clone() else {
            return;
        };
        let envelope = self.build_session_envelope(include_live);
        // Never fsync'd: it would block the main loop once a second through a
        // drag or a pan, and buys nothing against the crashes this file is for.
        if let Err(err) = session::write(&path, &envelope, false) {
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
        // transient keyboard focus doesn't erase the real one.
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
                let mut entry = suspended_entry(s, loc, self.suspended_chrome());
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
    /// stamped applied rule) so a hot-reload takes effect on the next write.
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
        // Recorded as the frame the stand-in will wear, like a live suspend's.
        let chrome = self.suspended_chrome();
        let frame = chrome.frame_size(body.size);
        let (x, y) = content_to_rule(body.loc, body.size, chrome);
        let id = *next_id;
        *next_id += 1;
        Some(SessionEntry {
            id,
            app_id: identity.app_id,
            desktop_id: identity.desktop_id,
            display_name: identity.display_name,
            position: [x, y],
            size: [frame.w, frame.h],
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

/// A durable entry for a suspended window at canvas position `loc`, recorded as
/// the visual frame it wears rather than the body it stores.
fn suspended_entry(s: &SuspendedWindow, loc: Point<i32, Logical>, chrome: Chrome) -> SessionEntry {
    let frame = chrome.frame_size(s.size.get());
    let (x, y) = content_to_rule(loc, s.size.get(), chrome);
    SessionEntry {
        id: s.id.0,
        app_id: s.identity.app_id.clone(),
        desktop_id: s.identity.desktop_id.clone(),
        display_name: s.identity.display_name.clone(),
        position: [x, y],
        size: [frame.w, frame.h],
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

/// Convert a schema-v1 entry, whose `position`/`size` describe the stand-in's
/// bare body, into the v2 convention where both describe its visible frame.
///
/// Uniform, with no per-entry branch: every stand-in wears the same bar and the
/// same default border, whatever its origin. The chrome comes from the config
/// this boot loaded, so a `title_bar_height` changed since the file was written
/// shifts a converted stand-in by half the difference — close enough for a
/// one-time conversion.
fn body_entry_to_frame(mut entry: SessionEntry, chrome: Chrome) -> SessionEntry {
    let body = Size::from((entry.size[0], entry.size[1]));
    let loc = rule_to_internal(entry.position[0], entry.position[1], body);
    let frame = chrome.frame_size(body);
    let (x, y) = content_to_rule(loc, body, chrome);
    entry.position = [x, y];
    entry.size = [frame.w, frame.h];
    entry
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
