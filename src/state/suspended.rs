//! Interaction and lifecycle for suspended windows (the compositor-drawn
//! stand-ins left behind when a window is suspended). Rendering lives in the
//! render module; this is focus, relaunch, and dismissal.
//!
//! Relaunch mints an activation token to spawn the app, then adopts the
//! returning window into the stand-in's slot; the pending-launch state (which
//! [`DriftWm::is_suspended_launching`] reads for the "launching…" label) is the
//! single owner, on `DriftWm`.

use std::collections::HashMap;
use std::rc::Rc;
use std::time::{Duration, Instant};

use smithay::desktop::Window;
use smithay::reexports::wayland_server::Resource;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{IsAlive, Logical, Rectangle, SERIAL_COUNTER, Size};
use smithay::wayland::seat::WaylandFocus;
use smithay::wayland::xdg_activation::XdgActivationToken;

use driftwm::desktop_entry::{AppIdentity, DesktopEntryCache};
use driftwm::stage::ElementId;
use driftwm::window_ext::WindowExt;

use crate::decorations::DecorationKey;
use crate::state::{DriftWm, StageWindow, SuspendedId, SuspendedWindow};
use crate::surface_tree::focus_belongs_to_toplevel;

/// A close whose `toplevel_destroyed` should convert into a suspended window,
/// recorded when `suspend-window` fires. The window is asked to close; the mark
/// carries what the stand-in needs, and lapses if the client refuses to close.
pub struct SuspendMark {
    pub identity: AppIdentity,
    /// Trigger-time body rect: content top-left (stage position) + geometry size.
    pub rect: Rectangle<i32, Logical>,
    pub title: String,
    pub deadline: Instant,
}

/// How long a suspend / real-close mark stays live. A client that refuses to
/// close (unsaved-changes dialog) within this window is treated as a normal
/// survivor: the mark lapses and a later close behaves per `suspend_on_close`.
const MARK_TTL: Duration = Duration::from_secs(10);

/// How long the identity fallback (Signal B) keeps matching a token-less new
/// window to a pending relaunch. Kept tight — token-ignoring clients map
/// quickly, and a short window shrinks the same-app capture hazard.
const FALLBACK_WINDOW: Duration = Duration::from_secs(5);

/// How long a pending relaunch lives before it is garbage-collected: the token
/// is deregistered and the "launching…" label reverts to the app name.
const RELAUNCH_TTL: Duration = Duration::from_secs(30);

/// Stamped into a compositor-minted activation token's `user_data` so the
/// relaunched window can be matched back to the suspended window it came from
/// (Signal A), ahead of the normal serial-gated activation path.
pub struct RelaunchMarker(pub SuspendedId);

/// One in-flight relaunch. The suspended window holds no pending state — this
/// is the single owner.
pub struct PendingRelaunch {
    /// The compositor-minted token, deregistered on every exit.
    token: XdgActivationToken,
    /// When the relaunch was spawned — FIFO ordering for the identity fallback.
    spawned_at: Instant,
    /// The next stage id at spawn time: only a window mapped at or after this
    /// (id `>=` it) can be the relaunched window. Guards the post-map token
    /// path against a single-instance app forwarding the startup id to its
    /// already-open window.
    spawn_element_id: ElementId,
    /// After this, the identity fallback stops matching (token match still works).
    fallback_deadline: Instant,
    /// After this, the whole pending relaunch is garbage-collected.
    deadline: Instant,
}

impl PendingRelaunch {
    /// Whether `id` names a window mapped since this relaunch was spawned.
    pub fn maps_new_window(&self, id: ElementId) -> bool {
        id >= self.spawn_element_id
    }
}

impl DriftWm {
    /// The suspended element with `id`, if it's on the stage.
    pub fn find_suspended(&self, id: SuspendedId) -> Option<Rc<SuspendedWindow>> {
        self.stage
            .windows()
            .filter_map(|w| w.suspended())
            .find(|s| s.id == id)
            .cloned()
    }

    /// Focus + raise a suspended window (its body was clicked/tapped). Focus is
    /// intent-only: a suspended window holds no seat keyboard focus.
    pub fn focus_and_raise_suspended(&mut self, id: SuspendedId) {
        let Some(s) = self.find_suspended(id) else {
            return;
        };
        let element = StageWindow::Suspended(s);
        self.stage.raise_with_children(&element);
        self.enforce_below_windows();
        let serial = SERIAL_COUNTER.next_serial();
        self.set_suspended_focus(id, serial);
    }

    /// Focus + raise a suspended window and, if it isn't already fully on
    /// screen, pan the active output's camera to center it (no zoom change).
    /// Backs `msg focus <id>` on a suspended window.
    pub fn navigate_to_suspended(&mut self, id: SuspendedId) {
        self.focus_and_raise_suspended(id);
        let Some(s) = self.find_suspended(id) else {
            return;
        };
        let element = StageWindow::Suspended(s.clone());
        if self.window_fully_in_viewport(&element) {
            return;
        }
        let Some(output) = self.active_output() else {
            return;
        };
        let loc = self.stage.position_of(&element).unwrap_or_default();
        let size = s.size.get();
        let bar = self.window_ssd_bar(&element);
        let vc = self.usable_center_screen_on(&output);
        let zoom = crate::state::output_state(&output).zoom;
        let target = driftwm::canvas::camera_to_center_window(loc, size, vc, zoom, bar);
        let center = smithay::utils::Point::from((
            loc.x as f64 + size.w as f64 / 2.0,
            loc.y as f64 - bar as f64 + (size.h as f64 + bar as f64) / 2.0,
        ));
        let mut os = crate::state::output_state(&output);
        os.momentum.stop();
        os.zoom_animation_anchor = Some(crate::state::ZoomAnimationAnchor {
            canvas: center,
            screen: vc,
        });
        os.camera_target = Some(target);
        os.zoom_target = Some(zoom);
    }

    /// Relaunch the app behind a suspended window: resolve its `Exec=`, mint a
    /// compositor-owned activation token stamped so the new window can be
    /// matched back, spawn the app with that token in the child env, and record
    /// the pending relaunch (the label flips to "launching…"). Returns `false`
    /// only when the app no longer resolves to a launchable entry (so `msg
    /// relaunch` can report it); an already-in-flight relaunch is a `true`
    /// no-op.
    pub fn relaunch_suspended(&mut self, id: SuspendedId) -> bool {
        let Some(s) = self.find_suspended(id) else {
            return true;
        };
        if self.pending_relaunches.contains_key(&id) {
            return true;
        }

        // Resolve the command fresh — the app may have been uninstalled since
        // the window was suspended.
        let desktop_id = s.identity.desktop_id.clone();
        let argv = {
            let cache = self.desktop_entry_cache.get_or_insert_with(|| {
                tracing::info!(
                    "desktop-entry cache used before warm completed; building synchronously"
                );
                DesktopEntryCache::from_env()
            });
            cache.refresh();
            cache.launch_command(&desktop_id)
        };
        let Some(argv) = argv else {
            tracing::info!(
                "relaunch of {id:?}: '{desktop_id}' no longer resolves to a launchable entry"
            );
            return false;
        };

        // Serial-less by design: `request_activation` honors the marker ahead
        // of its serial gate.
        let now = Instant::now();
        let token = {
            let (token, data) = self.xdg_activation_state.create_external_token(None);
            data.user_data
                .insert_if_missing_threadsafe(|| RelaunchMarker(id));
            token.clone()
        };
        self.pending_relaunches.insert(
            id,
            PendingRelaunch {
                token: token.clone(),
                spawned_at: now,
                spawn_element_id: self.stage.next_element_id(),
                fallback_deadline: now + FALLBACK_WINDOW,
                deadline: now + RELAUNCH_TTL,
            },
        );

        let (command, env) =
            relaunch_command_and_env(&argv, token.as_str(), &self.config.child_env);
        Self::spawn_relaunch(&command, &env);

        // The label reads the pending map — flip it to "launching…" now.
        self.mark_all_dirty();
        true
    }

    /// Whether a suspended window is mid-relaunch, for the "launching…" label.
    pub fn is_suspended_launching(&self, id: SuspendedId) -> bool {
        self.pending_relaunches.contains_key(&id)
    }

    /// End an in-flight relaunch: drop the pending entry and deregister its
    /// token so a late activation of it falls through to normal placement.
    fn cancel_pending_relaunch(&mut self, id: SuspendedId) {
        if let Some(pending) = self.pending_relaunches.remove(&id) {
            self.xdg_activation_state.remove_token(&pending.token);
        }
    }

    /// Garbage-collect pending relaunches whose 30s deadline has passed,
    /// deregistering their tokens and reverting the "launching…" label. Takes
    /// `now` explicitly so tests drive expiry deterministically; production
    /// passes the wall clock from the per-frame tick.
    pub fn sweep_pending_relaunches(&mut self, now: Instant) {
        let mut expired = Vec::new();
        self.pending_relaunches.retain(|_, p| {
            if now >= p.deadline {
                expired.push(p.token.clone());
                false
            } else {
                true
            }
        });
        if expired.is_empty() {
            return;
        }
        for token in &expired {
            self.xdg_activation_state.remove_token(token);
        }
        // A reverted label needs a redraw.
        self.mark_all_dirty();
    }

    /// The suspended window a freshly-mapped relaunched `window` should adopt,
    /// resolving both match signals. Signal A: an activation-token stash for
    /// this exact surface (authoritative — a stale stash means normal
    /// placement, never a fall-through to the identity fallback). Signal B: the
    /// oldest pending relaunch of the same app whose 5s fallback window is still
    /// open. Consumes the Signal-A stash.
    pub(crate) fn adoption_target(
        &mut self,
        root: &WlSurface,
        window: &Window,
    ) -> Option<SuspendedId> {
        if let Some(sid) = self.pending_adoptions.remove(root) {
            return (self.pending_relaunches.contains_key(&sid)
                && self.find_suspended(sid).is_some())
            .then_some(sid);
        }

        let app_id = window.app_id_or_class().unwrap_or_default();
        if app_id.is_empty() {
            // An app-id-less window would match a (never-happens) empty-identity
            // pending; skip rather than risk an accidental capture.
            return None;
        }
        let now = Instant::now();
        let mut candidates: Vec<(SuspendedId, Instant)> = self
            .pending_relaunches
            .iter()
            .filter(|(_, p)| now < p.fallback_deadline)
            .map(|(&sid, p)| (sid, p.spawned_at))
            .collect();
        candidates.retain(|(sid, _)| {
            self.find_suspended(*sid)
                .is_some_and(|s| s.identity.app_id == app_id)
        });
        // FIFO: earliest spawn wins; ties broken by id for determinism.
        candidates.sort_by_key(|(sid, spawned)| (*spawned, *sid));
        candidates.first().map(|(sid, _)| *sid)
    }

    /// Adopt `window` (a relaunched client's freshly-mapped toplevel) into
    /// suspended window `sid`: a compound stage op — remove the window's own
    /// fresh entry (its `ElementId` discarded), then `Stage::replace` the
    /// suspended entry so the window inherits its z-slot, `ElementId`, and
    /// canvas position, sized to the body rect. Purges the suspended chrome
    /// caches, moves focus intent onto the adopted window if the suspended held
    /// it, ends the pending relaunch, and writes the session through. Camera is
    /// untouched; the caller sends the body-size configure.
    pub(crate) fn adopt_relaunched(&mut self, window: &Window, root: &WlSurface, sid: SuspendedId) {
        let Some(s) = self.find_suspended(sid) else {
            return;
        };
        let suspended = StageWindow::Suspended(s.clone());
        let pos = self.stage.position_of(&suspended).unwrap_or_default();
        let body_size = s.size.get();

        // Inherit the suspended window's focus if it held it (a relaunch the
        // user is waiting on ends up focused); focus that already moved on is
        // left where it is.
        let refocus = matches!(
            self.window_focus,
            Some(crate::state::FocusIntent::Suspended(held)) if held == sid
        ) || self
            .window_focus_surface()
            .is_some_and(|t| focus_belongs_to_toplevel(&t.0, root));

        // Compound replace: the fresh entry must leave before the suspended
        // entry is replaced, or the same window would sit in two z-slots and
        // trip the duplicate-window invariant.
        self.stage.remove(&StageWindow::Client(window.clone()));
        self.stage
            .replace(&suspended, StageWindow::Client(window.clone()));
        self.stage.set_position(window, pos);
        // The adopted window restores (fit/fullscreen round-trips) to the body.
        self.stage.set_restore_size_if_missing(window, body_size);

        // Fill the suspended body rect. The caller decides when the configure
        // is sent (first-commit path folds it into the initial configure).
        if let Some(toplevel) = window.toplevel() {
            toplevel.with_pending_state(|state| {
                state.size = Some(body_size);
            });
        }

        // Drop the suspended chrome caches; the adopted client renders its own.
        self.decorations.remove(&DecorationKey::Suspended(sid));
        self.render
            .border_cache
            .remove(&DecorationKey::Suspended(sid));
        self.render
            .shadow_cache
            .remove(&DecorationKey::Suspended(sid));

        self.cancel_pending_relaunch(sid);

        if refocus {
            let serial = SERIAL_COUNTER.next_serial();
            self.set_window_focus(Some(crate::state::FocusTarget(root.clone())), serial);
            // The `remove` above dropped the window from MRU history; if it was
            // already the seat focus (post-map path) the `set_focus` is a no-op
            // and `focus_changed` won't re-add it, so push it back explicitly.
            self.update_focus_history(root);
        }
        self.refresh_pointer_focus();
        // An adopt is an immediate, user-visible change — write through now.
        self.session_store_write_now();
    }

    #[cfg(not(test))]
    fn spawn_relaunch(command: &str, env: &HashMap<String, String>) {
        crate::state::spawn_command(command, env);
    }

    #[cfg(test)]
    fn spawn_relaunch(command: &str, env: &HashMap<String, String>) {
        // Tests drive the relaunched client by hand and must never fork the real
        // app; record the request so a scenario can assert on it.
        TEST_SPAWNS.with(|spawns| spawns.borrow_mut().push((command.to_string(), env.clone())));
    }

    /// Dismiss (close) a suspended window: drop it from the stage and its chrome
    /// caches, then run the same focus-follow a real window close does.
    pub fn dismiss_suspended(&mut self, id: SuspendedId) {
        let Some(s) = self.find_suspended(id) else {
            return;
        };
        // A dismiss mid-relaunch cancels it: a late token then finds no live
        // pending and falls through to normal placement.
        self.cancel_pending_relaunch(id);
        let was_focused = matches!(
            self.window_focus,
            Some(crate::state::FocusIntent::Suspended(sid)) if sid == id
        );

        self.stage.remove(&StageWindow::Suspended(s));
        self.decorations.remove(&DecorationKey::Suspended(id));
        self.render
            .border_cache
            .remove(&DecorationKey::Suspended(id));
        self.render
            .shadow_cache
            .remove(&DecorationKey::Suspended(id));

        if was_focused {
            // Close-style follow: return to the most-recent live window, panning
            // only if it isn't already fully on screen.
            let follow = self
                .stage
                .focus_history()
                .iter()
                .filter_map(|w| w.client())
                .find(|w| w.alive())
                .cloned();
            let serial = SERIAL_COUNTER.next_serial();
            match follow {
                Some(target) if self.window_fully_in_viewport(&target) => {
                    self.raise_and_focus(&target, serial);
                }
                Some(target) => self.navigate_to_window(&target, false),
                None => self.set_window_focus(None, serial),
            }
        }
        // The suspended window may have sat under the cursor; re-target so a
        // click no longer lands in dead space.
        self.refresh_pointer_focus();
        // A dismiss is an immediate, user-visible change — write through now.
        self.session_store_write_now();
    }

    /// The pre-fullscreen restore rect (saved location + size) of the focused
    /// window, if it is fullscreen. Captured by the action dispatcher *before*
    /// the fullscreen-exit prelude, because the client keeps reporting the
    /// fullscreen buffer size until it acks the exit configure — reading its
    /// geometry afterwards would size the stand-in to the whole screen.
    pub fn focused_fullscreen_restore_rect(&self) -> Option<Rectangle<i32, Logical>> {
        let window = self.focused_window()?;
        let output = window
            .wl_surface()
            .and_then(|s| self.find_fullscreen_output_for_surface(&s))?;
        let entry = self.stage.fullscreen_on(&output.name())?;
        Some(Rectangle::new(entry.saved_location, entry.saved_size))
    }

    /// The `suspend-window` action: close the focused window but arrange for a
    /// suspended stand-in to take its place. `restore_rect` is the pre-fullscreen
    /// rect captured before the prelude's fullscreen exit; identity is
    /// pre-resolved so a no-`.desktop` window closes honestly instead of
    /// vanishing forever.
    pub fn suspend_focused_window(&mut self, restore_rect: Option<Rectangle<i32, Logical>>) {
        // Dialogs and modals are ineligible — same exclusion the
        // `suspend_on_close` path applies. Suspending one would relaunch a
        // whole fresh app instance, which is nonsense for a child dialog.
        let Some(window) = self
            .focused_window()
            .filter(|w| !w.is_widget() && w.parent_surface().is_none() && !w.is_modal())
        else {
            return;
        };
        // The prelude only exits fullscreen on the active output; cover a
        // window fullscreen elsewhere.
        if let Some(output) = window
            .wl_surface()
            .and_then(|s| self.find_fullscreen_output_for_surface(&s))
        {
            self.exit_fullscreen_on(&output);
        }
        // Land a pinned window back on the canvas first, so the stand-in is a
        // normal canvas window at the spot the user sees it.
        if self.is_pinned(&window) {
            self.unpin_to_canvas(&window);
        }
        let Some(surface) = window.wl_surface().map(|s| s.into_owned()) else {
            return;
        };
        let app_id = window.app_id_or_class().unwrap_or_default();
        let Some(identity) = self.resolve_identity(&app_id) else {
            tracing::info!(
                "suspend-window: '{app_id}' resolves to no .desktop entry; closing normally"
            );
            self.mark_real_close(&window);
            window.send_close();
            return;
        };

        // The stand-in's body rect: the pre-fullscreen restore rect if the
        // window was fullscreen, else the current windowed geometry — the fit
        // / current visual size wins, restore size dropped.
        let rect = restore_rect.unwrap_or_else(|| {
            let loc = self.stage.position_of(&window).unwrap_or_default();
            Rectangle::new(loc, window.geometry().size)
        });
        let title = window.window_title().unwrap_or_default();
        self.suspend_marks.insert(
            surface.id(),
            SuspendMark {
                identity,
                rect,
                title,
                deadline: Instant::now() + MARK_TTL,
            },
        );
        window.send_close();
    }

    /// Unpin `window` back to a canvas position at the current camera/zoom (no
    /// visual jump), mirroring the `toggle-pin-to-screen` landing.
    fn unpin_to_canvas(&mut self, window: &Window) {
        let Some(site) = self.stage.take_pin(window) else {
            return;
        };
        let Some(output) = self.output_by_name(&site.output) else {
            return;
        };
        let (camera, zoom) = {
            let os = crate::state::output_state(&output);
            (os.camera, os.zoom)
        };
        let canvas = driftwm::canvas::screen_to_canvas(
            driftwm::canvas::ScreenPos(site.screen_pos.to_f64()),
            camera,
            zoom,
        )
        .0
        .to_i32_round();
        self.map_window(window.clone(), canvas, true);
    }

    /// Mark a window's next destroy as a real close so `suspend_on_close`
    /// doesn't convert it. Same TTL + sweep as suspend marks, so a refused
    /// close can't real-close an unrelated crash days later.
    pub fn mark_real_close(&mut self, window: &Window) {
        if let Some(surface) = window.wl_surface() {
            self.mark_real_close_surface(&surface);
        }
    }

    pub fn mark_real_close_surface(&mut self, surface: &WlSurface) {
        self.real_close_marks
            .insert(surface.id(), Instant::now() + MARK_TTL);
    }

    /// Resolve a surface `app_id` to a launchable identity, using the warmed
    /// desktop-entry cache (built synchronously on the first miss if the warm
    /// hasn't landed). Refreshes on directory-mtime change (cheap).
    pub fn resolve_identity(&mut self, app_id: &str) -> Option<AppIdentity> {
        let cache = self.desktop_entry_cache.get_or_insert_with(|| {
            tracing::info!(
                "desktop-entry cache used before warm completed; building synchronously"
            );
            DesktopEntryCache::from_env()
        });
        cache.refresh();
        cache.resolve(app_id)
    }

    /// The effective `suspend_on_close` for `(app_id, title)`: a matching window
    /// rule's override wins, else the global default. Resolved live (not the
    /// stamped applied rule) so hot-reload takes effect immediately.
    fn resolve_suspend_on_close(&self, app_id: &str, title: &str) -> bool {
        self.config
            .resolve_window_rules(app_id, title)
            .and_then(|r| r.suspend_on_close)
            .unwrap_or(self.config.suspend_on_close)
    }

    /// Decide whether a destroying `window` converts into a suspended window,
    /// returning the stand-in's identity + geometry + title if so. Marks decide
    /// first: with both a suspend and a real-close mark live (two conflicting
    /// commands on a close-refusing window), the later one wins — deadlines are
    /// set-time plus a shared TTL, so comparing them compares set order. With
    /// no live mark, an eligible client-initiated close converts when
    /// `suspend_on_close` resolves true.
    ///
    /// `suspend_on_close` eligibility: not a widget, not a dialog (no parent —
    /// dead or alive — and not modal), resolves to a `.desktop` entry, and the
    /// resolved flag is on.
    pub fn resolve_suspend_conversion(
        &mut self,
        surface: &WlSurface,
        window: &Window,
        fullscreen_restore_rect: Option<Rectangle<i32, Logical>>,
    ) -> Option<SuspendConversion> {
        // Consume both marks up front so neither leaks past this destroy, but
        // honor them only while unexpired — an idle event loop may dispatch this
        // destroy before the per-frame sweep culls a lapsed mark.
        let now = Instant::now();
        let real_close_deadline = self
            .real_close_marks
            .remove(&surface.id())
            .filter(|deadline| *deadline > now);
        let suspend_mark = self
            .suspend_marks
            .remove(&surface.id())
            .filter(|mark| mark.deadline > now);
        if let Some(real) = real_close_deadline
            && suspend_mark
                .as_ref()
                .is_none_or(|mark| mark.deadline < real)
        {
            return None;
        }
        // Read while the surface is still live (before `cleanup_surface_state`):
        // an SSD window has a decoration entry, a CSD one doesn't. The stand-in
        // keeps the same footprint — bar + body, or body only.
        let has_bar = self
            .decorations
            .contains_key(&DecorationKey::Surface(surface.id()));
        if let Some(mark) = suspend_mark {
            return Some(SuspendConversion {
                identity: mark.identity,
                rect: mark.rect,
                title: mark.title,
                has_bar,
            });
        }

        if window.is_widget() || window.parent_surface().is_some() || window.is_modal() {
            return None;
        }
        let app_id = window.app_id_or_class().unwrap_or_default();
        let title = window.window_title().unwrap_or_default();
        if !self.resolve_suspend_on_close(&app_id, &title) {
            return None;
        }
        let identity = self.resolve_identity(&app_id)?;
        // A fullscreen self-close reports the fullscreen buffer size at its
        // camera park, not the windowed rect — the pre-fullscreen saved rect
        // (same source the explicit action and the shutdown serializer use)
        // seats the stand-in where the window actually was.
        let rect =
            fullscreen_restore_rect.unwrap_or_else(|| self.markless_suspend_rect(window, surface));
        Some(SuspendConversion {
            identity,
            rect,
            title,
            has_bar,
        })
    }

    /// Body rect for a `suspend_on_close` conversion. Destroy-time
    /// `window.geometry()` can't be trusted (foot shrinks its buffer while
    /// tearing down), so a `stable_snap_rects` entry — deflated back to a body
    /// size — wins, but only when the live geometry actually shrank; otherwise
    /// live is authoritative (the cached rect can be stale). With no cached
    /// rect, fall back to the stage's restore size.
    fn markless_suspend_rect(
        &self,
        window: &Window,
        surface: &WlSurface,
    ) -> Rectangle<i32, Logical> {
        let loc = self.stage.position_of(window).unwrap_or_default();
        let live = window.geometry().size;

        let stable = self.stable_snap_rects.get(&surface.id()).map(|r| {
            let bar = self.window_ssd_bar(window);
            let bw = self.window_border_width(surface);
            Size::from((
                (r.x_high - r.x_low) as i32 - 2 * bw,
                (r.y_high - r.y_low) as i32 - bar - 2 * bw,
            ))
        });

        let size = match stable {
            Some(stable) if live.w < stable.w || live.h < stable.h => stable,
            Some(_) => live,
            None => self
                .stage
                .restore_size(window)
                .filter(|s| s.w > 0 && s.h > 0)
                .unwrap_or(live),
        };
        Rectangle::new(loc, size)
    }

    /// Replace a destroying client window with a suspended stand-in in place:
    /// same z-slot and `ElementId`, at the recorded rect. Runs the conversion
    /// cleanup checklist before `cleanup_surface_state` wipes the surface state.
    pub fn convert_to_suspended(
        &mut self,
        window: &Window,
        surface: &WlSurface,
        conv: SuspendConversion,
    ) {
        // The suspended window inherits the keyboard focus intent only if the
        // dying client held it — a background close must not steal focus.
        let was_focused = self
            .window_focus_surface()
            .is_some_and(|t| focus_belongs_to_toplevel(&t.0, surface));

        let sid = SuspendedId(self.next_suspended_id);
        self.next_suspended_id += 1;
        // A live suspend (explicit action or suspend_on_close) is an explicit,
        // user-visible artifact, so it always returns on restore.
        let suspended = Rc::new(SuspendedWindow::new(
            sid,
            conv.rect.size,
            conv.identity,
            conv.title,
            driftwm::session::Origin::Explicit,
            conv.has_bar,
        ));
        let new_element = StageWindow::Suspended(suspended);

        // Cleanup checklist: drop the dead client from focus history, clear the
        // per-entry fit / restore / pin state (a stand-in has none), and seat
        // the stand-in at the recorded position. The surface-keyed decoration /
        // border / shadow / stable-rect / pending entries are purged by the
        // following `cleanup_surface_state`; suspended chrome renders lazily
        // under the `Suspended` key.
        self.stage.drop_from_focus_history(window);
        self.stage.clear_fit(window);
        self.stage.clear_restore_size(window);
        self.stage.clear_fill(window);
        self.stage.take_pin(window);
        self.stage.set_position(window, conv.rect.loc);
        self.stage.replace(window, new_element);

        if was_focused {
            let serial = SERIAL_COUNTER.next_serial();
            self.set_suspended_focus(sid, serial);
        }

        self.refresh_pointer_focus();
        // A create is an immediate, user-visible change — write through now
        // rather than on the debounce timer (move/resize use that).
        self.session_store_write_now();
    }

    /// Drop suspend / real-close marks whose deadline has passed. Takes `now`
    /// explicitly so tests drive expiry deterministically; production passes the
    /// wall clock from the per-frame tick.
    pub fn sweep_marks(&mut self, now: Instant) {
        self.suspend_marks.retain(|_, mark| mark.deadline > now);
        self.real_close_marks.retain(|_, deadline| *deadline > now);
    }

    /// Kick off the desktop-entry scan on a background thread so the first
    /// suspend never cold-parses hundreds of `.desktop` files on the input
    /// thread. A ping delivers the finished cache on completion (the
    /// `warm_fonts` pattern). Production-only — tests inject their own cache;
    /// a suspend before the warm lands builds one synchronously.
    pub fn warm_desktop_entry_cache(&mut self) {
        use smithay::reexports::calloop::ping::make_ping;
        use std::sync::{Arc, Mutex};

        let slot: Arc<Mutex<Option<DesktopEntryCache>>> = Arc::new(Mutex::new(None));
        let (ready_ping, ready_source) = match make_ping() {
            Ok(pair) => pair,
            Err(err) => {
                tracing::warn!("failed to create desktop-entry warm ping: {err}");
                return;
            }
        };
        let slot_for_handler = slot.clone();
        let inserted =
            self.loop_handle
                .insert_source(ready_source, move |_, _, data: &mut DriftWm| {
                    // Keep a synchronously-built cache if a suspend already forced
                    // one before the warm landed.
                    if let Some(cache) = slot_for_handler.lock().unwrap().take()
                        && data.desktop_entry_cache.is_none()
                    {
                        data.desktop_entry_cache = Some(cache);
                    }
                });
        if let Err(err) = inserted {
            tracing::warn!("failed to insert desktop-entry warm ping source: {err}");
            return;
        }
        let spawned = std::thread::Builder::new()
            .name("driftwm-desktop-entry-warm".into())
            .spawn(move || {
                *slot.lock().unwrap() = Some(DesktopEntryCache::from_env());
                ready_ping.ping();
            });
        if let Err(err) = spawned {
            tracing::warn!("failed to spawn desktop-entry warm thread: {err}");
        }
    }
}

/// The identity + geometry + title a conversion hands to the new stand-in.
pub struct SuspendConversion {
    pub identity: AppIdentity,
    pub rect: Rectangle<i32, Logical>,
    pub title: String,
    /// Whether the dying window was SSD (compositor bar) — the stand-in keeps a
    /// bar to match, or is body-only for a CSD window.
    pub has_bar: bool,
}

/// Build the `sh -c` command line and child environment for a relaunch. The
/// activation token is exported under both env-var names clients read
/// (`XDG_ACTIVATION_TOKEN` / `DESKTOP_STARTUP_ID`), layered over the config's
/// child env. `spawn_command` runs the string through `sh -c`, so each argv
/// token is shell-quoted to survive whitespace and metacharacters.
fn relaunch_command_and_env(
    argv: &[String],
    token: &str,
    child_env: &HashMap<String, String>,
) -> (String, HashMap<String, String>) {
    let command = argv
        .iter()
        .map(|arg| shell_quote(arg))
        .collect::<Vec<_>>()
        .join(" ");
    let mut env = child_env.clone();
    env.insert("XDG_ACTIVATION_TOKEN".to_string(), token.to_string());
    env.insert("DESKTOP_STARTUP_ID".to_string(), token.to_string());
    (command, env)
}

/// POSIX single-quote a shell word: wrap in single quotes, closing and escaping
/// each embedded quote as `'\''`. Safe for any argv token.
fn shell_quote(arg: &str) -> String {
    let mut out = String::with_capacity(arg.len() + 2);
    out.push('\'');
    for c in arg.chars() {
        if c == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(c);
        }
    }
    out.push('\'');
    out
}

#[cfg(test)]
thread_local! {
    /// Relaunch spawns recorded in place of forking (see `spawn_relaunch`).
    /// Per-thread, so each test's fixture sees only its own spawns.
    static TEST_SPAWNS: std::cell::RefCell<Vec<(String, HashMap<String, String>)>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

#[cfg(test)]
impl DriftWm {
    /// Materialize a suspended window at `pos` (content top-left) sized `size`,
    /// raised to the top of the z-order. Production never constructs a suspended
    /// element this way — chunk 4 owns conversion — so this exists only to
    /// exercise rendering, hit-testing, and focus in isolation.
    pub fn insert_suspended_for_test(
        &mut self,
        id: u64,
        pos: smithay::utils::Point<i32, smithay::utils::Logical>,
        size: smithay::utils::Size<i32, smithay::utils::Logical>,
        app_id: &str,
        display_name: &str,
    ) -> SuspendedId {
        let sid = SuspendedId(id);
        let identity = driftwm::desktop_entry::AppIdentity {
            app_id: app_id.to_string(),
            desktop_id: app_id.to_string(),
            display_name: display_name.to_string(),
        };
        let s = Rc::new(SuspendedWindow::new(
            sid,
            size,
            identity,
            display_name.to_string(),
            driftwm::session::Origin::Explicit,
            true,
        ));
        self.map_window(StageWindow::Suspended(s), pos, true);
        sid
    }

    /// As [`Self::insert_suspended_for_test`], but body-only (a CSD-origin
    /// stand-in): no compositor title bar, footprint == the body rect.
    pub fn insert_suspended_barless_for_test(
        &mut self,
        id: u64,
        pos: smithay::utils::Point<i32, smithay::utils::Logical>,
        size: smithay::utils::Size<i32, smithay::utils::Logical>,
        app_id: &str,
        display_name: &str,
    ) -> SuspendedId {
        let sid = SuspendedId(id);
        let identity = driftwm::desktop_entry::AppIdentity {
            app_id: app_id.to_string(),
            desktop_id: app_id.to_string(),
            display_name: display_name.to_string(),
        };
        let s = Rc::new(SuspendedWindow::new(
            sid,
            size,
            identity,
            display_name.to_string(),
            driftwm::session::Origin::Explicit,
            false,
        ));
        self.map_window(StageWindow::Suspended(s), pos, true);
        sid
    }

    /// The activation-token string minted for a pending relaunch, for a fixture
    /// client to present via `xdg_activation.activate`.
    pub fn pending_relaunch_token_for_test(&self, id: SuspendedId) -> Option<String> {
        self.pending_relaunches
            .get(&id)
            .map(|p| p.token.as_str().to_string())
    }

    /// Backdate a pending relaunch's fallback window into the past, so a
    /// token-less same-app window no longer adopts it (the identity fallback
    /// expired) while the relaunch itself is still pending.
    pub fn expire_relaunch_fallback_for_test(&mut self, id: SuspendedId) {
        if let Some(p) = self.pending_relaunches.get_mut(&id) {
            p.fallback_deadline = Instant::now() - Duration::from_secs(1);
        }
    }

    /// Drain the relaunch spawns recorded on this thread since the last drain.
    pub fn take_relaunch_spawns_for_test(&self) -> Vec<(String, HashMap<String, String>)> {
        TEST_SPAWNS.with(|spawns| std::mem::take(&mut *spawns.borrow_mut()))
    }

    /// Build a stand-in's body + label chrome the way the render pass does, but
    /// with an explicit `fonts_ready` (the render thread reads the global font
    /// state). Returns the label's cache key, so a test can assert the label
    /// re-rasters once fonts arrive. The buffers need no GL renderer.
    pub fn build_suspended_chrome_for_test(
        &self,
        id: SuspendedId,
        launching: bool,
        fonts_ready: bool,
    ) -> Option<(i32, i32, i32, bool, bool)> {
        let s = self.find_suspended(id)?;
        let size = s.size.get();
        crate::render::ensure_body(
            &s,
            size,
            self.decoration_scale,
            !s.has_bar,
            &self.config.decorations,
        );
        crate::render::ensure_label(
            &s,
            size,
            self.decoration_scale,
            launching,
            fonts_ready,
            &self.config.decorations,
        );
        s.chrome.borrow().label_key
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relaunch_command_shell_quotes_and_sets_both_token_vars() {
        let mut child_env = HashMap::new();
        child_env.insert("EXISTING".to_string(), "1".to_string());
        let argv = vec![
            "my app".to_string(),
            "--flag".to_string(),
            "a'b".to_string(),
        ];
        let (command, env) = relaunch_command_and_env(&argv, "TOK123", &child_env);
        assert_eq!(command, r#"'my app' '--flag' 'a'\''b'"#);
        assert_eq!(env["XDG_ACTIVATION_TOKEN"], "TOK123");
        assert_eq!(env["DESKTOP_STARTUP_ID"], "TOK123");
        // The child env is preserved.
        assert_eq!(env["EXISTING"], "1");
    }

    #[test]
    fn shell_quote_wraps_plain_words() {
        assert_eq!(shell_quote("firefox"), "'firefox'");
        assert_eq!(shell_quote(""), "''");
    }
}
