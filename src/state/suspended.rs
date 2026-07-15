//! Interaction and lifecycle for suspended windows (the compositor-drawn
//! stand-ins left behind when a window is suspended). Rendering lives in the
//! render module; this is focus, relaunch, and dismissal.
//!
//! Relaunch is a stub here — chunk 5 (relaunch + matching) fills
//! [`DriftWm::relaunch_suspended`] and the pending-launch state that
//! [`DriftWm::is_suspended_launching`] reads.

use std::rc::Rc;
use std::time::{Duration, Instant};

use smithay::desktop::Window;
use smithay::reexports::wayland_server::Resource;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{IsAlive, Logical, Rectangle, SERIAL_COUNTER, Size};
use smithay::wayland::seat::WaylandFocus;

use driftwm::desktop_entry::{AppIdentity, DesktopEntryCache};
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
        os.zoom_animation_center = Some(center);
        os.camera_target = Some(target);
        os.zoom_target = Some(zoom);
    }

    /// Relaunch the app behind a suspended window. Stub: chunk 5 mints the
    /// activation token, spawns via the resolved `Exec=`, and drives adoption.
    pub fn relaunch_suspended(&mut self, id: SuspendedId) {
        if self.find_suspended(id).is_none() {
            return;
        }
        tracing::info!("relaunch of suspended window {id:?} requested (not yet wired)");
    }

    /// Whether a suspended window is mid-relaunch, for the "launching…" label.
    /// Stub: chunk 5 tracks pending relaunches; nothing is pending yet.
    pub fn is_suspended_launching(&self, _id: SuspendedId) -> bool {
        false
    }

    /// Dismiss (close) a suspended window: drop it from the stage and its chrome
    /// caches, then run the same focus-follow a real window close does.
    pub fn dismiss_suspended(&mut self, id: SuspendedId) {
        let Some(s) = self.find_suspended(id) else {
            return;
        };
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
        let Some(window) = self.focused_window().filter(|w| !w.is_widget()) else {
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
        if let Some(mark) = suspend_mark {
            return Some(SuspendConversion {
                identity: mark.identity,
                rect: mark.rect,
                title: mark.title,
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
        Some(SuspendConversion {
            identity,
            rect: self.markless_suspend_rect(window, surface),
            title,
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
        ));
        self.map_window(StageWindow::Suspended(s), pos, true);
        sid
    }
}
