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
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
use smithay::reexports::wayland_server::Resource;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{IsAlive, Logical, Point, Rectangle, SERIAL_COUNTER, Size};
use smithay::wayland::compositor::{BufferAssignment, SurfaceAttributes, with_states};
use smithay::wayland::seat::WaylandFocus;
use smithay::wayland::xdg_activation::XdgActivationToken;

use driftwm::desktop_entry::{AppIdentity, DesktopEntryCache};
use driftwm::window_ext::WindowExt;

use crate::decorations::DecorationKey;
use crate::grabs::ResizeState;
use crate::state::{DriftWm, StageWindow, SuspendedId, SuspendedWindow};
use crate::surface_tree::focus_belongs_to_toplevel;

/// A close whose `toplevel_destroyed` should convert into a suspended window,
/// recorded when `suspend-window` fires. The window is asked to close; the mark
/// carries what the stand-in needs, and lapses if the client refuses to close.
pub struct SuspendMark {
    pub identity: AppIdentity,
    /// Trigger-time body rect: content top-left (stage position) + geometry size.
    pub rect: Rectangle<i32, Logical>,
    pub deadline: Instant,
}

/// The markless-conversion inputs captured the instant a mapped toplevel
/// unmaps (a null-buffer commit). smithay resets the xdg role on unmap, so by
/// the time `toplevel_destroyed` runs on a client that unmaps before destroying,
/// the app_id / title / parent / geometry are all gone — an eligible close then
/// resolves to an empty identity and vanishes instead of leaving a stand-in.
/// The snapshot is consumed by the destroy that follows and dropped if the
/// surface remaps (an app that unmaps to hide must never leave a stand-in).
pub struct UnmapSnapshot {
    pub app_id: String,
    pub title: String,
    pub is_widget: bool,
    pub has_parent: bool,
    pub is_modal: bool,
    pub rect: Rectangle<i32, Logical>,
    pub csd: bool,
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

/// Which path stashed a deferred adopt, which decides how much of the carve-out
/// list still applies when it lands. The activation path meets an already-placed
/// window, so every carve-out is a live question about where that window ended
/// up. The first-commit path resolves adoption *ahead* of window rules, so an
/// adopt stashed there has already beaten them and only a membership acquired
/// during the deferral can still call it off — the placement block keeps the
/// membership arms off for as long as the stash holds the surface, so that
/// membership can only come from the client or the user.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum AdoptOrigin {
    FirstCommit,
    Activation,
}

/// Why a hidden window is being put back on screen, which decides how much of
/// the placement tail the hiding withheld it still gets.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum RevealCause {
    /// The grab let go and the adopt lands in this same dispatch: it teleports
    /// the window, draws its own arrival, and re-seats focus itself.
    Adopt,
    /// The grab let go but the adopt can no longer land, so the window stays
    /// where it was placed — the release is still the moment the user is
    /// waiting for it to arrive.
    Released,
    /// Nothing released: the stash was reclaimed under a user who has been
    /// working elsewhere, for as long as the relaunch deadline.
    Abandoned,
}

/// One adopt a live grab held back. Four things drain it — the grab-release
/// flush, the per-frame liveness sweep, `cleanup_surface_state`, and a second
/// deferral superseding it — and every one of the first three goes through
/// [`DriftWm::reveal_deferred_adopt`], because a first-commit entry is also
/// what keeps its window off the screen.
pub struct DeferredAdopt {
    /// The relaunched window's root surface.
    pub root: WlSurface,
    pub sid: SuspendedId,
    pub origin: AdoptOrigin,
}

/// One in-flight relaunch. The suspended window holds no pending state — this
/// is the single owner.
pub struct PendingRelaunch {
    /// The compositor-minted token, deregistered on every exit.
    token: XdgActivationToken,
    /// When the relaunch was spawned — FIFO ordering for the identity fallback.
    spawned_at: Instant,
    /// After this, the identity fallback stops matching (token match still works).
    fallback_deadline: Instant,
    /// After this, the whole pending relaunch is garbage-collected.
    deadline: Instant,
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
        // Go through the `DriftWm` wrapper (not `stage` directly) so the previous
        // client's xdg `activated` hint clears when focus moves to the stand-in.
        self.raise_with_children(&element);
        self.enforce_below_windows();
        let serial = SERIAL_COUNTER.next_serial();
        self.set_suspended_focus(id, serial);
    }

    /// Focus + raise a suspended window and, if it isn't already fully on
    /// screen, pan the active output's camera to center it (no zoom change).
    /// Backs `msg focus <id>` on a suspended window — the centering actions want
    /// `center_on_suspended` instead, which always centers.
    pub fn reveal_suspended(&mut self, id: SuspendedId) {
        self.focus_and_raise_suspended(id);
        let Some(s) = self.find_suspended(id) else {
            return;
        };
        if self.window_fully_in_viewport(&StageWindow::Suspended(s.clone())) {
            return;
        }
        let Some(output) = self.active_output() else {
            return;
        };
        let zoom = crate::state::output_state(&output).zoom;
        self.center_camera_on_suspended(&s, &output, zoom);
    }

    /// Focus + raise a suspended window and center the active output's camera on
    /// it unconditionally — the stand-in counterpart of `navigate_to_window`;
    /// `reset_zoom` matches its meaning there.
    pub fn center_on_suspended(&mut self, id: SuspendedId, reset_zoom: bool) {
        self.focus_and_raise_suspended(id);
        let Some(s) = self.find_suspended(id) else {
            return;
        };
        let Some(output) = self.active_output() else {
            return;
        };
        let zoom = self.navigation_target_zoom(&output, reset_zoom);
        self.center_camera_on_suspended(&s, &output, zoom);
    }

    fn center_camera_on_suspended(
        &mut self,
        s: &Rc<SuspendedWindow>,
        output: &smithay::output::Output,
        target_zoom: f64,
    ) {
        let element = StageWindow::Suspended(s.clone());
        let loc = self.stage.position_of(&element).unwrap_or_default();
        let size = s.size.get();
        let bar = self.window_ssd_bar(&element);
        let vc = self.usable_center_screen_on(output);
        let target = driftwm::canvas::camera_to_center_window(loc, size, vc, target_zoom, bar);
        let center = self.nav_center(&element);
        let mut os = crate::state::output_state(output);
        os.overview_return = None;
        os.momentum.stop();
        os.zoom_animation_anchor = Some(crate::state::ZoomAnimationAnchor {
            canvas: center,
            screen: vc,
        });
        os.camera_target = Some(target);
        os.zoom_target = Some(target_zoom);
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

    /// An element mid-interactive-move or -resize is being driven by a live
    /// grab, so nothing may reposition it out from under that grab: adopting a
    /// window into a stand-in slot would be fought by the next motion (which
    /// snaps it back, with button-up reseeding the snap rect), and an animation
    /// entry would re-seed its leg on every motion and rubber-band behind the
    /// cursor. Unlike the durable fullscreen/pinned/widget/dialog carve-outs
    /// this is transient, so the relaunch caller stashes the adopt for the
    /// grab's release rather than dismissing.
    pub(crate) fn element_under_interactive_grab(&self, element: &StageWindow) -> bool {
        if self.element_under_interactive_move(element) {
            return true;
        }
        // The resize half is a client-side protocol state, so it answers for
        // clients only; a stand-in's resize grab is compositor-side.
        element.wl_surface().is_some_and(|surface| {
            with_states(&surface, |states| {
                !matches!(
                    *states
                        .data_map
                        .get_or_insert(|| std::cell::RefCell::new(ResizeState::Idle))
                        .borrow(),
                    ResizeState::Idle
                )
            })
        })
    }

    /// Whether adopting into `sid` would fight a live grab — on the window being
    /// adopted, or on the stand-in whose slot it takes. Either side is destroyed
    /// or teleported by the adopt, which leaves the grab that was driving it
    /// pushing air until the button comes back up.
    pub(crate) fn adopt_fights_a_grab(&self, window: &Window, sid: SuspendedId) -> bool {
        self.element_under_interactive_grab(&StageWindow::Client(window.clone()))
            || self
                .find_suspended(sid)
                .is_some_and(|s| self.element_under_interactive_grab(&StageWindow::Suspended(s)))
    }

    /// Resolve an adopt of already-placed `window` into stand-in `sid` into one
    /// of three outcomes: adopt it, defer it to
    /// [`Self::flush_deferred_adoptions`] while a grab is live, or dismiss the
    /// stand-in because the window has landed somewhere the adopt would rip it
    /// out of.
    pub(crate) fn resolve_placed_adopt(
        &mut self,
        window: &Window,
        root: &WlSurface,
        sid: SuspendedId,
        origin: AdoptOrigin,
    ) {
        // Transient, unlike the carve-outs below: the pending relaunch keeps
        // running so the grab's release can still land the adopt. Answered
        // first because the flush re-runs this for *every* stashed entry and
        // any grab release anywhere schedules it — a carve-out decided ahead of
        // this check would dismiss a stand-in the user is still dragging, over
        // a membership the relaunched window took meanwhile. The flush asks the
        // same question one step earlier, so that it can leave the entry in
        // place and hold its window hidden across the renewed deferral; the two
        // must keep one answer.
        if self.adopt_fights_a_grab(window, sid) {
            self.defer_adoption(root, sid, origin);
            return;
        }
        // A window already fullscreen or pinned is where policy put it; adopting
        // would rip it out of that membership, so drop the stand-in instead and
        // leave the window alone. A rule-placed widget, and a dialog/modal owned
        // by another window, are the same call — but they are decided by window
        // rules and the xdg parent link, both of which the first-commit path
        // resolves *after* it has already picked the adopt (every suspend path
        // excludes dialogs, so no stand-in ever stands for one). An adopt
        // stashed there beat them once and must not lose to them at the flush.
        let beats_rules = origin == AdoptOrigin::FirstCommit;
        if self.is_window_fullscreen(window)
            || self.is_pinned(window)
            || (!beats_rules
                && (window.is_widget() || window.parent_surface().is_some() || window.is_modal()))
        {
            tracing::debug!(
                "relaunch adopt of {sid:?} skipped: window is fullscreen/pinned/widget/dialog; dismissing stand-in"
            );
            self.dismiss_suspended(sid);
            return;
        }
        self.adopt_relaunched(window, root, sid);
        if let Some(toplevel) = window.toplevel() {
            toplevel.send_configure();
        }
    }

    /// Stash an adopt a live grab held back. One window can only ever adopt one
    /// stand-in, so a second match on the same surface supersedes the first and
    /// the superseded relaunch falls back to its TTL. Order is insertion order,
    /// so two windows of one app racing for the same stand-in resolve by which
    /// deferred first rather than by hash order.
    ///
    /// Keyed on the surface alone, so a later placement pass — where the token
    /// stash is spent and only the identity fallback can still match — may
    /// supersede the entry with a *different* stand-in of the same app (FIFO
    /// over the pending relaunches), retargeting the adopt away from the one the
    /// token named.
    pub(crate) fn defer_adoption(
        &mut self,
        root: &WlSurface,
        sid: SuspendedId,
        origin: AdoptOrigin,
    ) {
        match self.deferred_adoptions.iter_mut().find(|d| d.root == *root) {
            // The target is superseded, the origin is not — in either
            // direction. It decides whether the entry hides its window, so
            // swapping a first-commit entry for an activation pops the window
            // into view mid-drag and then skips the reveal the drain owes it,
            // and swapping the other way hides a window the user has been
            // looking at all along. The first verdict also already beat the
            // window rules, and that stands.
            Some(slot) => slot.sid = sid,
            None => self.deferred_adoptions.push(DeferredAdopt {
                root: root.clone(),
                sid,
                origin,
            }),
        }
    }

    /// Whether a stashed adopt still has both ends — the pending relaunch it
    /// was minted for, and the stand-in whose slot it would take. Either gone
    /// and it can never land, whatever the grab does.
    pub(crate) fn relaunch_target_live(&self, sid: SuspendedId) -> bool {
        self.pending_relaunches.contains_key(&sid) && self.find_suspended(sid).is_some()
    }

    /// Whether a relaunched window rooted at `surface` is waiting on a deferred
    /// adopt that has not landed yet. Such a window is staged and placed, but
    /// the placement is a holding pattern the flush will teleport it out of, so
    /// it is kept off the screen and out of every canvas relation until then —
    /// drawing it would show the user a window at a site it is about to leave,
    /// on top of whatever is really there.
    ///
    /// Only the first-commit origin hides: the activation origin defers an
    /// *already placed* window (the flush's own re-check aside, the handler
    /// stashes only when the surface has left `pending_center`), which the user
    /// has been looking at — and possibly dragging — all along.
    ///
    /// The stash itself is the flag, so nothing can desync from the reveal.
    pub(crate) fn root_hidden_by_deferred_adopt(&self, surface: &WlSurface) -> bool {
        self.deferred_adoptions
            .iter()
            .any(|d| d.root == *surface && d.origin == AdoptOrigin::FirstCommit)
    }

    /// [`Self::root_hidden_by_deferred_adopt`] for callers holding an element
    /// rather than a surface. Guarded on the (near-always empty) stash first, so
    /// the surface lookup never runs on the per-motion hit-test walks.
    pub(crate) fn hidden_by_deferred_adopt<Q: WaylandFocus>(&self, element: &Q) -> bool {
        !self.deferred_adoptions.is_empty()
            && element
                .wl_surface()
                .is_some_and(|s| self.root_hidden_by_deferred_adopt(&s))
    }

    /// Whether a client's fullscreen / maximize request has to be queued for
    /// someone else to apply rather than taken now. Before the first sized
    /// commit the geometry is still (0,0), which poisons the saved size and
    /// lets the placement clobber the result. While a deferred adopt hides the
    /// window there is nothing drawn to fullscreen: the render skip and the
    /// fullscreen cull would between them empty the output — a black screen
    /// until the button comes up — and a fit would pan the camera onto a rect
    /// nothing is drawn at. Both queues are applied by whoever clears the
    /// condition, the placement or the reveal.
    pub(crate) fn queues_geometry_request(&self, surface: &WlSurface) -> bool {
        self.pending_center.contains(surface) || self.root_hidden_by_deferred_adopt(surface)
    }

    /// Put a window the stash was hiding back on screen at the placement it has
    /// been holding. Everything the suppression withheld has to be handed over
    /// here, because the placement pass that would have done it is long past.
    pub(crate) fn reveal_deferred_adopt(
        &mut self,
        root: &WlSurface,
        origin: AdoptOrigin,
        cause: RevealCause,
    ) {
        if origin != AdoptOrigin::FirstCommit {
            return;
        }
        // Nothing to show for a surface whose window has already left the stage,
        // or one being torn down — the drain is the whole of the reveal there.
        // Load-bearing for `cleanup_surface_state`, which reveals *after*
        // removing the per-surface entries this writes back: the crash route
        // reaches it with the window still staged, and only the dead surface
        // stops the reveal from re-seeding what was just cleared.
        let Some(window) = self.window_for_surface(root).filter(|_| root.alive()) else {
            return;
        };
        // Not on the adopt path: `Stage::replace` hands the window the stand-in's
        // `ElementId`, and the adopt drops the animation entries for both ids on
        // the way, so an open armed here on the pre-replace id is never read. The
        // stand-in crossfading into the slot is that path's arrival.
        if cause != RevealCause::Adopt {
            self.start_window_open_animation(&window);
        }
        // The placement's own refresh found no rect to write (a hidden window
        // has no snap rect at all), and on every path but the adopt nothing else
        // ever writes one — leaving the window outside the reflow's grow test
        // and without the shrink protection its close reads.
        let client = StageWindow::Client(window.clone());
        self.refresh_stable_snap_rect(&client);
        // The placement's raise/focus was suppressed with the rest, so the
        // window is absent from the focus history entirely: Alt-Tab skips it,
        // and an adopt that follows finds no history slot to restore. Widgets
        // and `focus_on_open = false` keep their exemption.
        let may_focus = driftwm::config::applied_rule(root)
            .is_none_or(|r| !r.widget && r.focus_on_open != Some(false));
        // The background arm of the placement never focuses or raises a window
        // it tucked behind a fullscreen one, and the reveal stands in for that
        // placement. Asked against the membership as it is now, since the
        // fullscreen may well have exited during the deferral.
        let behind_fullscreen = self
            .output_for_window(&window)
            .is_some_and(|o| self.is_output_fullscreen(&o));
        if may_focus {
            if cause == RevealCause::Abandoned || behind_fullscreen {
                // An abandoned reveal has let the keyboard move on for a whole
                // relaunch deadline, and a window tucked behind a fullscreen one
                // was never promised the focus in the first place. Both still owe
                // it the cycle, joined at the far end rather than the front.
                // Written to the history directly because that is a back
                // insertion rather than a promotion — but on the normal writer's
                // terms: its eligibility filter, and its freeze while a cycle is
                // walking the list.
                if self.enters_focus_history(&client) && self.stage.cycle_state().is_none() {
                    let back = self.stage.focus_history().len();
                    self.stage.restore_focus_history_at(&client, back);
                }
            } else {
                let serial = SERIAL_COUNTER.next_serial();
                self.raise_and_focus(&window, serial);
            }
        }
        // A fullscreen or maximize the client asked for while it was hidden was
        // queued instead of applied. An abandoned reveal is the one cause that
        // must not hand it over: nothing was released, so the drag that forced
        // the deferral may still be running, and entering fullscreen would flip
        // the screen to an app the user never saw arrive, take the keyboard off
        // what they were typing into, and park a camera the drag is still
        // pushing. There is no later pass to hold the request for, so it goes
        // with the entry that was abandoned.
        if cause == RevealCause::Abandoned {
            self.pending_fullscreen.remove(root);
            self.pending_fit.remove(root);
        }
        // An adopt lands in this same dispatch and teleports the window into the
        // stand-in's slot, so the request waits for that: a fit measured here
        // frames the holding placement and parks the camera on a rect the
        // teleport then moves the window out of, and a fullscreen entered here
        // saves the holding placement as the rect its exit restores — losing the
        // slot the user dragged the stand-in to. The flush hands it over once
        // the window has the slot.
        if cause != RevealCause::Adopt {
            self.apply_queued_geometry_request(root);
        }
        // The window is hit-testable again from here, under a pointer that may
        // not have moved since it was placed. Last, so the answer is taken
        // against the rect the request above moved it to; the adopt route takes
        // it again after its own teleport and its own request.
        self.refresh_pointer_focus();
    }

    /// Hand over a fullscreen / maximize the client asked for while its window
    /// was hidden and [`Self::queues_geometry_request`] queued instead of
    /// applying. Fullscreen wins when a client asked for both. Reports whether
    /// anything was queued, so a caller that has already taken pointer focus can
    /// tell whether the window moved out from under it.
    pub(crate) fn apply_queued_geometry_request(&mut self, root: &WlSurface) -> bool {
        let Some(window) = self.window_for_surface(root) else {
            return false;
        };
        let fullscreen = self.pending_fullscreen.remove(root);
        let fit = fullscreen.is_none() && self.pending_fit.remove(root);
        let applied = fullscreen.is_some() || fit;
        if let Some(client_output) = fullscreen {
            let target = self.resolve_fullscreen_output(root, client_output);
            self.enter_fullscreen(&window, target);
        } else if fit {
            // The fit configures its own size, so a stable rect the adopt is
            // still owed — payable only on a commit at the adopted size — would
            // never come due, and `decoration_fit` writes none of its own: the
            // window would end up with no settled footprint at all. Pay it here
            // against the slot the adopt put it in, which is exactly the pre-fit
            // rect a fit means to keep as the window's cluster identity. A
            // fullscreen needs no such payment — its exit configures the adopted
            // size back and the debt settles then, at a rect that exists again.
            self.settle_owed_adopt_rect(&window, root);
            self.decoration_fit(&window);
        }
        applied
    }

    /// Pay off the stable snap rect an adopt is owing, at the rect the adopt put
    /// the window in rather than the size the client has committed — for a
    /// caller about to move the window somewhere the debt's own settle can never
    /// be reached from.
    fn settle_owed_adopt_rect(&mut self, window: &Window, root: &WlSurface) {
        let Some(adopt_size) = self.pending_adopt_settle.remove(&root.id()) else {
            return;
        };
        let Some(loc) = self.stage.position_of(window) else {
            return;
        };
        self.cache_stable_snap_rect(window, loc, adopt_size);
    }

    /// Take `root` out of the stash and hand back what the hiding withheld,
    /// immediately before the adopt that ends it. Revealed under the adopt's own
    /// cause, so the reveal leaves the crossfade and the queued geometry request
    /// to the adopt that follows.
    fn end_hiding_for_adopt(&mut self, root: &WlSurface) {
        let Some(idx) = self.deferred_adoptions.iter().position(|d| d.root == *root) else {
            return;
        };
        let entry = self.deferred_adoptions.remove(idx);
        self.reveal_deferred_adopt(&entry.root, entry.origin, RevealCause::Adopt);
    }

    /// Drop the stashed adopts that can never land — their pending relaunch was
    /// swept past its deadline, their stand-in was dismissed, or an adopt of the
    /// same stand-in cancelled the relaunch out from under a second entry — and
    /// reveal the windows they were hiding.
    ///
    /// Runs on the per-frame tick because nothing else revisits a stashed entry:
    /// the flush fires only off a grab release, and a deferral can outlive every
    /// grab there is (a client resize whose client never commits again holds one
    /// with no grab live at all). Without this a window could stay invisible for
    /// the rest of the session; with it the relaunch TTL bounds the wait.
    pub fn sweep_deferred_adoptions(&mut self) {
        if self.deferred_adoptions.is_empty() {
            return;
        }
        let abandoned: Vec<WlSurface> = self
            .deferred_adoptions
            .iter()
            .filter(|d| !self.relaunch_target_live(d.sid))
            .map(|d| d.root.clone())
            .collect();
        for root in abandoned {
            let Some(idx) = self.deferred_adoptions.iter().position(|d| d.root == root) else {
                continue;
            };
            // Taken out one at a time, each immediately before its own reveal: a
            // reveal walks every window (pointer focus, snap rects), and a second
            // entry waiting its turn has to still read as hidden there.
            let entry = self.deferred_adoptions.remove(idx);
            self.reveal_deferred_adopt(&entry.root, entry.origin, RevealCause::Abandoned);
        }
    }

    /// Queue the adoptions a grab held back for the moment the current dispatch
    /// unwinds. The adopt re-seats pointer focus and a grab's teardown runs
    /// inside the pointer mutex, so it can't run inline from there. Called from
    /// every point a grab this stash can wait on releases: a move grab's disarm,
    /// and the commit that settles a client resize back to `ResizeState::Idle`.
    pub(crate) fn schedule_deferred_adoptions(&mut self) {
        if self.deferred_adoptions.is_empty() {
            return;
        }
        self.loop_handle
            .insert_idle(|data| data.flush_deferred_adoptions());
    }

    /// Land the deferred adoptions whose grab has let go. Each entry re-runs the
    /// full decision, so one still held by a second grab simply stays stashed
    /// rather than blocking the rest; it rides that second grab's own scheduling
    /// point, and deliberately does not re-arm an idle here — an always-pending
    /// idle makes the event loop spin.
    ///
    /// A relaunch the TTL swept while the grab was held is not revived: the
    /// window keeps the placement it already has and the stand-in stays behind
    /// as a stale duplicate — the end state of any relaunch that outlives its
    /// deadline.
    pub(crate) fn flush_deferred_adoptions(&mut self) {
        // Walked over the live stash by root rather than over a drained copy: a
        // reveal walks every window (pointer focus, snap rects), and an entry
        // still waiting its turn has to read as hidden while that runs. Each
        // root is taken at most once, so an entry re-deferred below rides the
        // next flush instead of looping here.
        let roots: Vec<WlSurface> = self
            .deferred_adoptions
            .iter()
            .map(|d| d.root.clone())
            .collect();
        for root in roots {
            let Some(idx) = self.deferred_adoptions.iter().position(|d| d.root == root) else {
                continue;
            };
            let (sid, origin) = (
                self.deferred_adoptions[idx].sid,
                self.deferred_adoptions[idx].origin,
            );
            // No stage window behind the surface — destroyed, or unmapped to
            // hide — so the entry leaves with the drain rather than waiting for
            // a remap: the stand-in stays as a stale duplicate, and a window
            // that does come back is placed fresh.
            let Some(window) = self.window_for_surface(&root) else {
                self.deferred_adoptions.remove(idx);
                continue;
            };
            let target_live = self.relaunch_target_live(sid);
            // Asked here rather than left to `resolve_placed_adopt`, which
            // answers it the same way: a second grab holding either side keeps
            // the entry where it is, and the window has to stay hidden across
            // that instead of being revealed and hidden again in one dispatch.
            if target_live && self.adopt_fights_a_grab(&window, sid) {
                continue;
            }
            self.deferred_adoptions.remove(idx);
            // Ahead of the adopt, and in the same dispatch as it: the window is
            // visible from here on, and no frame may be composed showing it at
            // the placement it is about to be teleported out of.
            let cause = if target_live {
                RevealCause::Adopt
            } else {
                RevealCause::Released
            };
            self.reveal_deferred_adopt(&root, origin, cause);
            // A relaunch the TTL swept, or a stand-in dismissed, while the grab
            // was held leaves the window where the reveal just put it.
            if !target_live {
                continue;
            }
            self.resolve_placed_adopt(&window, &root, sid, origin);
            // The reveal held this back so the adopt could take the slot first;
            // it applies against the rect the window actually ended up with,
            // whether the adopt landed or a carve-out dropped the stand-in. Only
            // a first-commit entry ever hides its window, so only that origin
            // can have a queued request to hand over.
            if origin == AdoptOrigin::FirstCommit && self.apply_queued_geometry_request(&root) {
                // Both the reveal and the adopt took pointer focus before this
                // moved the window again, under a pointer that has not moved
                // since: the answer has to be taken once more at the rect the
                // request leaves it at.
                self.refresh_pointer_focus();
            }
        }
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
        // The adopt is the end of the hiding: from here the window holds the
        // stand-in's slot on screen, and the raise and focus below are the ones
        // it keeps. Left stashed, the primitives would refuse the adopt's own
        // refocus and leave the keyboard aimed at a stand-in the `replace` is
        // about to consume. The flush drains its entry a step earlier, but a
        // later placement pass and a re-presented relaunch token both arrive
        // here with the entry still in — so the invariant is asserted where the
        // adopt is, not at each route into it.
        self.end_hiding_for_adopt(root);
        let suspended = StageWindow::Suspended(s.clone());
        let pos = self.stage.position_of(&suspended).unwrap_or_default();
        let body_size = s.size.get();
        let bar_i = self.window_ssd_bar(&suspended);

        // A CSD-origin stand-in shrank its body under the bar at conversion, so
        // hand the app back the full window: positioned above the body by the
        // current bar height, sized to include it. If the bar height changed
        // while suspended the outer rect drifts by the difference — the same
        // drift a live SSD window sees on reload, so no special handling. An
        // SSD-origin adopt keeps the body rect; its SSD bar re-attaches via the
        // normal decoration path.
        let (adopt_pos, adopt_size) = if s.csd {
            (
                Point::from((pos.x, pos.y - bar_i)),
                Size::from((body_size.w, body_size.h + bar_i)),
            )
        } else {
            (pos, body_size)
        };

        // Inherit the suspended window's focus if it held it (a relaunch the
        // user is waiting on ends up focused); focus that already moved on is
        // left where it is.
        let refocus = matches!(
            self.window_focus,
            Some(crate::state::FocusIntent::Suspended(held)) if held == sid
        ) || self
            .window_focus_surface()
            .is_some_and(|t| focus_belongs_to_toplevel(&t.0, root));

        // `remove` below drops the adopted window from the focus history; the
        // refocus path re-seats it at the front, but a non-refocus adopt (an
        // already-open window forwarding the token while a newer window holds
        // focus) must keep its prior MRU slot or the window silently vanishes
        // from the Alt-Tab cycle until it is next focused. Capture the slot now.
        let client = StageWindow::Client(window.clone());
        let history_slot = self.stage.focus_history().iter().position(|w| *w == client);

        // Crossfade the departing stand-in over the window that takes its slot
        // (backend-gated — headless never accumulates render transients). Drop
        // animation entries for both involved ids: the discarded fresh entry and
        // the suspended entry the window inherits.
        if self.backend.is_some() {
            // Read before the relaunch is cancelled and focus moves below, so the
            // fade keeps the chrome the user was actually looking at.
            let launching = self.is_suspended_launching(sid);
            let focused = self.gated_suspended_focus() == Some(sid);
            self.standin_fades.push(crate::render::StandInFade {
                suspended: s.clone(),
                loc: pos,
                launching,
                focused,
                // A representation exchange, not a close: alpha only.
                shrink: 1.0,
                progress: 0.0,
            });
        }
        if let Some(id) = self.stage.id_of(&client) {
            self.window_animations.remove(id);
            self.drop_resize_crossfade(id);
        }
        if let Some(id) = self.stage.id_of(&suspended) {
            self.window_animations.remove(id);
            self.drop_resize_crossfade(id);
        }

        // The adopt places the window in the stand-in's slot; the adopt
        // configure's own resize is exactly the commit an owed recenter would
        // complete on.
        self.drop_owed_recenter(window);

        // Compound replace: the fresh entry must leave before the suspended
        // entry is replaced, or the same window would sit in two z-slots and
        // trip the duplicate-window invariant.
        self.stage.remove(&StageWindow::Client(window.clone()));
        self.stage
            .replace(&suspended, StageWindow::Client(window.clone()));
        self.stage.set_position(window, adopt_pos);
        // The adopted window restores (fit/fullscreen round-trips) to its
        // reassembled size.
        self.stage.set_restore_size_if_missing(window, adopt_size);
        // Owe the stable snap rect rather than assert one now: the client is
        // still committing whatever size it mapped with, so any rect written
        // here is a footprint it has not drawn. An entry left over from the
        // window's pre-adopt slot is just as stale, so it goes too — the commit
        // handler seeds off the first frame that actually carries `adopt_size`.
        self.stable_snap_rects.remove(&root.id());
        self.pending_adopt_settle.insert(root.id(), adopt_size);

        // Fill the reassembled window rect. The caller decides when the
        // configure is sent (first-commit path folds it into the initial
        // configure).
        if let Some(toplevel) = window.toplevel() {
            toplevel.with_pending_state(|state| {
                state.size = Some(adopt_size);
                // The window's own entry left with the `remove` above, taking any
                // fit state with it, and the inherited one never had any — so a
                // Maximized left set here is one the client can never shed: its
                // restore button would dispatch an unmaximize_request that
                // `unfit_window` silently drops.
                state.states.unset(xdg_toplevel::State::Maximized);
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
            // Activation is no longer granted at birth, and adoption skips the
            // normal placement's activation, so stage the adopted window's
            // Activated hint here — it rides the decoration-tail configure the
            // caller sends, and any displaced peer is deactivated on the wire.
            self.activate_riding_batch(window);
        } else if let Some(idx) = history_slot {
            self.stage.restore_focus_history_at(&client, idx);
        }
        self.refresh_pointer_focus();

        // Hold the adopted rect from the first frame: the client is still
        // committing buffers at whatever size it mapped with until it acks the
        // configure the caller is about to send, so without this it draws
        // undersized beneath the fading stand-in chrome — a flicker instead of a
        // crossfade. Seeding the chase with from == target holds the slot until
        // the ack lands, then it bends to the real geometry.
        self.begin_geometry_animation_seeded(
            window,
            Rectangle::new(adopt_pos.to_f64(), adopt_size.to_f64()),
            crate::state::window_animation::AnimSpace::Canvas,
            Some(adopt_size),
            crate::state::window_animation::GeometryRole::Normal,
            // The window has inherited this slot, so fill it: capping here would
            // render it undersized in the corner of the slot under the crossfade.
            crate::state::window_animation::ContentPolicy::Stretch,
            None,
        );

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
        // Read before the cancel below clears it, so the fade freezes the label
        // the user was looking at when they dismissed.
        let launching = self.is_suspended_launching(id);
        // A dismiss mid-relaunch cancels it: a late token then finds no live
        // pending and falls through to normal placement.
        self.cancel_pending_relaunch(id);
        let was_focused = matches!(
            self.window_focus,
            Some(crate::state::FocusIntent::Suspended(sid)) if sid == id
        );

        // Fade the stand-in out like a real window close. The chrome textures
        // live on the Rc, so retaining it here — before the cache evictions
        // below — keeps them renderable after the stage entry is gone. Skipped
        // when it would never be seen (headless, or off every drawable output).
        let element = StageWindow::Suspended(s.clone());
        // Resolved while the stand-in is still on the stage — its rect is what
        // the cluster/overlap and nearest-visible queries below are relative to.
        let (follow, home, center) = if was_focused {
            let follow = self
                .first_spatially_related_in_history(&element)
                // An off-screen follow is only worth panning to when the user
                // asked for that; otherwise focus must stay somewhere visible.
                .filter(|t| self.config.auto_navigate_on_close || self.window_fully_in_viewport(t));
            let home = self
                .output_for_window(&element)
                .or_else(|| self.active_output());
            let center = self
                .visual_frame_rect(&element)
                .map(|r| Point::from(((r.x_low + r.x_high) / 2.0, (r.y_low + r.y_high) / 2.0)));
            (follow, home, center)
        } else {
            (None, None, None)
        };

        // Only the location is taken from the animation's in-flight visual: the
        // fade draws the chrome at the stand-in's own size, so asking about any
        // other size would answer a question the frame composer never poses.
        let loc = self
            .departing_standin_rect(&element)
            .map(|r| r.loc.to_i32_round());
        if self.backend.is_some()
            && let Some(loc) = loc
            && self.canvas_rect_drawable(Rectangle::new(loc, s.size.get()))
        {
            let focused = self.gated_suspended_focus() == Some(id);
            self.standin_fades.push(crate::render::StandInFade {
                suspended: s.clone(),
                loc,
                launching,
                focused,
                shrink: self.config.effects.animation_scale,
                progress: 0.0,
            });
        }

        // The tick reaps an entry whose id no longer resolves, but only on the
        // next tick — one stale frame of a slide that has nothing left to slide.
        if let Some(eid) = self.stage.id_of(&element) {
            self.window_animations.remove(eid);
            self.drop_resize_crossfade(eid);
        }

        self.stage.remove(&element);
        self.decorations.remove(&DecorationKey::Suspended(id));
        self.render
            .border_cache
            .remove(&DecorationKey::Suspended(id));
        self.render
            .shadow_cache
            .remove(&DecorationKey::Suspended(id));

        if was_focused {
            // Mirrors a real close's follow tiers, minus the parent tier (a
            // stand-in is an app-level slot, so no xdg parent link exists): a
            // spatially related history entry first, else a visible MRU window
            // on the stand-in's home output, else the nearest visible one to
            // where it sat, else nothing. That fallback arm never pans.
            let serial = SERIAL_COUNTER.next_serial();
            match follow {
                Some(target) if self.window_fully_in_viewport(&target) => {
                    self.raise_and_focus(&target, serial);
                }
                Some(target) => self.navigate_to_window(&target, false),
                None => {
                    let mru = self
                        .stage
                        .focus_history()
                        .iter()
                        .filter_map(|w| w.client())
                        .find(|w| w.alive())
                        .cloned();
                    let target = match (home.as_ref(), mru) {
                        (Some(out), Some(m)) if self.window_intersects_viewport_on(&m, out) => {
                            Some(m)
                        }
                        (Some(out), _) => {
                            center.and_then(|c| self.nearest_visible_window_on(c, out, None))
                        }
                        (None, _) => None,
                    };
                    match target {
                        Some(target) => self.raise_and_focus(&target, serial),
                        None => self.set_window_focus(None, serial),
                    }
                }
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
        let Some(window) = self.focused_window() else {
            return;
        };
        self.suspend_window(&window, restore_rect);
    }

    /// [`Self::suspend_focused_window`] for a caller that already holds the
    /// window. The IPC verb needs it: its selector can name a window the
    /// keyboard is not allowed to reach (one still hidden for a deferred adopt),
    /// and resolving the target through focus would suspend somebody else's.
    pub fn suspend_window(
        &mut self,
        window: &Window,
        restore_rect: Option<Rectangle<i32, Logical>>,
    ) {
        // Dialogs and modals are ineligible — same exclusion the
        // `suspend_on_close` path applies. Suspending one would relaunch a
        // whole fresh app instance, which is nonsense for a child dialog.
        if window.is_widget() || window.parent_surface().is_some() || window.is_modal() {
            return;
        }
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
        if self.is_pinned(window) {
            self.unpin_to_canvas(window);
        }
        let Some(surface) = window.wl_surface().map(|s| s.into_owned()) else {
            return;
        };
        let app_id = window.app_id_or_class().unwrap_or_default();
        let Some(identity) = self.resolve_identity(&app_id) else {
            tracing::info!(
                "suspend-window: '{app_id}' resolves to no .desktop entry; closing normally"
            );
            self.mark_real_close(window);
            window.send_close();
            return;
        };

        // The stand-in's body rect: the pre-fullscreen restore rect if the
        // window was fullscreen, else the current windowed geometry — the fit
        // / current visual size wins, restore size dropped.
        let rect = restore_rect.unwrap_or_else(|| {
            let loc = self.stage.position_of(window).unwrap_or_default();
            Rectangle::new(loc, window.geometry().size)
        });
        self.suspend_marks.insert(
            surface.id(),
            SuspendMark {
                identity,
                rect,
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

    /// Runs as a pre-commit hook: snapshot the markless-conversion inputs the
    /// instant a mapped toplevel unmaps, before the xdg role reset on the null
    /// buffer wipes them. Registered ahead of the role-reset hook, so the reads
    /// here still see the pre-unmap identity, geometry, and decoration state.
    /// A remap (a fresh buffer) drops any stale snapshot; every other commit is
    /// a no-op.
    pub fn capture_unmap_snapshot(&mut self, surface: &WlSurface) {
        enum Change {
            Unmap,
            Remap,
            Other,
        }
        let change = with_states(surface, |states| {
            match states
                .cached_state
                .get::<SurfaceAttributes>()
                .pending()
                .buffer
            {
                Some(BufferAssignment::Removed) => Change::Unmap,
                Some(BufferAssignment::NewBuffer(_)) => Change::Remap,
                None => Change::Other,
            }
        });
        match change {
            Change::Unmap => {}
            Change::Remap => {
                // An app that unmaps to hide and shows itself again must never
                // leave a stand-in behind on a later close.
                self.unmap_snapshots.remove(&surface.id());
                return;
            }
            Change::Other => return,
        }

        let Some(window) = self.window_for_surface(surface) else {
            return;
        };
        // The initial commit can also carry a null buffer; only a currently
        // mapped window (non-zero geometry, still intact this side of the role
        // reset) is genuinely unmapping.
        let live = window.geometry().size;
        if live.w <= 0 || live.h <= 0 {
            return;
        }
        // A CSD window has no decoration entry; the stand-in records that origin
        // so adopt can hand back the full geometry (the bar it grows shrinks the
        // body, not the footprint).
        let csd = self.surface_is_csd(surface);
        let rect = self.markless_suspend_rect(&window, surface);
        self.unmap_snapshots.insert(
            surface.id(),
            UnmapSnapshot {
                app_id: window.app_id_or_class().unwrap_or_default(),
                title: window.window_title().unwrap_or_default(),
                is_widget: window.is_widget(),
                has_parent: window.parent_surface().is_some(),
                is_modal: window.is_modal(),
                rect,
                csd,
            },
        );
    }

    /// One entry point for the close-animation capture: on buffer removal the
    /// OLD buffer's textures are still what surface state returns, so clone them
    /// for the eventual flatten. Renderer-gated (the flatten needs one anyway),
    /// and invalidated on remap — the unmap hook fires on every hide, not just
    /// closes, so a hide-to-tray app must not pin stale textures.
    pub fn capture_close_pixels_on_unmap(&mut self, surface: &WlSurface) {
        enum Change {
            Unmap,
            Remap,
            Other,
        }
        let change = with_states(surface, |states| {
            match states
                .cached_state
                .get::<SurfaceAttributes>()
                .pending()
                .buffer
            {
                Some(BufferAssignment::Removed) => Change::Unmap,
                Some(BufferAssignment::NewBuffer(_)) => Change::Remap,
                None => Change::Other,
            }
        });
        let id = surface.id();
        match change {
            Change::Remap => {
                self.close_pixels.remove(&id);
                return;
            }
            Change::Other => return,
            Change::Unmap => {}
        }
        // First capture wins for this surface.
        if self.close_pixels.contains_key(&id) {
            return;
        }
        // Record the geometry alongside the pixels: this is the last moment it is
        // readable, since the null-buffer commit this hook precedes collapses it.
        let Some(geometry) = self.window_for_surface(surface).map(|w| w.geometry()) else {
            return;
        };
        let Some(mut backend) = self.backend.take() else {
            return;
        };
        if let Some(pixels) = crate::render::capture_close_pixels(
            backend.renderer(),
            surface,
            geometry,
            Instant::now(),
        ) {
            self.close_pixels.insert(id, pixels);
        }
        self.backend = Some(backend);
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
            .unwrap_or(self.config.session.suspend_on_close)
    }

    /// Decide whether a destroying `window` converts into a suspended window,
    /// returning the stand-in's identity + geometry if so. Marks decide
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
        // A client that unmaps before destroying loses its role state on the
        // unmap commit; the snapshot taken then carries the eligibility inputs
        // and pre-unmap footprint that the live reads below can no longer see.
        // Consumed here whichever branch wins, so it never outlives the destroy.
        let snapshot = self.unmap_snapshots.remove(&surface.id());
        if let Some(real) = real_close_deadline
            && suspend_mark
                .as_ref()
                .is_none_or(|mark| mark.deadline < real)
        {
            return None;
        }
        // An SSD window has a decoration entry, a CSD one doesn't. Every stand-in
        // keeps the original footprint, but the origin decides how the geometry
        // is reassembled on adopt. The decoration map can flip during an
        // unmap-before-destroy teardown, so the snapshot's pre-unmap truth wins
        // where present; otherwise the live read (still valid this side of
        // `cleanup_surface_state`).
        let csd = snapshot
            .as_ref()
            .map(|s| s.csd)
            .unwrap_or_else(|| self.surface_is_csd(surface));
        if let Some(mark) = suspend_mark {
            return Some(SuspendConversion {
                identity: mark.identity,
                rect: mark.rect,
                csd,
            });
        }
        // Below the mark, which carries its own rect and an explicit "suspend
        // this": a window still hidden for a deferred adopt has never been on
        // screen and the stand-in it was bound for is still standing, so
        // converting it *on its own* would leave the user two stand-ins for one
        // app — the second at a holding placement, faded in from nothing, and
        // sized by guesswork since the hiding is why no stable snap rect was
        // written.
        if self.root_hidden_by_deferred_adopt(surface) {
            return None;
        }

        // Eligibility + identity read from the snapshot when the surface unmapped
        // before destroying (the live reads are wiped by then), else live.
        let (is_widget, has_parent, is_modal, app_id, title) = match &snapshot {
            Some(s) => (
                s.is_widget,
                s.has_parent,
                s.is_modal,
                s.app_id.clone(),
                s.title.clone(),
            ),
            None => (
                window.is_widget(),
                window.parent_surface().is_some(),
                window.is_modal(),
                window.app_id_or_class().unwrap_or_default(),
                window.window_title().unwrap_or_default(),
            ),
        };
        if is_widget || has_parent || is_modal {
            return None;
        }
        if !self.resolve_suspend_on_close(&app_id, &title) {
            return None;
        }
        let identity = self.resolve_identity(&app_id)?;
        // A fullscreen self-close reports the fullscreen buffer size at its
        // camera park, not the windowed rect — the pre-fullscreen saved rect
        // (same source the explicit action and the shutdown serializer use)
        // seats the stand-in where the window actually was. Failing that, the
        // pre-unmap snapshot rect, then the live markless rect.
        let rect = fullscreen_restore_rect
            .or_else(|| snapshot.as_ref().map(|s| s.rect))
            .unwrap_or_else(|| self.markless_suspend_rect(window, surface));
        Some(SuspendConversion {
            identity,
            rect,
            csd,
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

    /// Whether the window behind `surface` is client-decorated — the single
    /// source of truth for the stand-in `csd` flag, shared by conversion and
    /// quit-save so they can't drift. An SSD window owns a `decorations` entry
    /// keyed by its surface; a CSD one never does. (Reading the entry, not
    /// `window_ssd_bar == 0`, keeps it correct even at a zero title-bar height.)
    pub(crate) fn surface_is_csd(&self, surface: &WlSurface) -> bool {
        !self
            .decorations
            .contains_key(&DecorationKey::Surface(surface.id()))
    }

    /// The stand-in's stage rect (its body) for a conversion. An SSD origin
    /// keeps the content rect — the compositor bar sits above it. A CSD origin
    /// shrinks the body under a bar of the current height so the outer footprint
    /// (bar + body) still equals the original window rect. A window shorter than
    /// the bar clamps to a 1px body: preserving the footprint would give a
    /// zero/negative body, so the footprint grows by the shortfall instead.
    pub(crate) fn standin_body_rect(
        &self,
        rect: Rectangle<i32, Logical>,
        csd: bool,
    ) -> Rectangle<i32, Logical> {
        if !csd {
            return rect;
        }
        let bar = self.config.decorations.title_bar_height;
        let body_h = (rect.size.h - bar).max(1);
        Rectangle::new(
            Point::from((rect.loc.x, rect.loc.y + bar)),
            Size::from((rect.size.w, body_h)),
        )
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

        // Drop any window animation entry: `stage.replace` preserves the id, so
        // the stand-in would otherwise inherit a stale client chase — or wear the
        // dead client's crossfade.
        if let Some(id) = self.stage.id_of(window) {
            self.window_animations.remove(id);
            self.drop_resize_crossfade(id);
        }

        let sid = SuspendedId(self.next_suspended_id);
        self.next_suspended_id += 1;
        // A CSD origin shrinks its body under the bar so the outer footprint is
        // unchanged; an SSD one already had a bar above its content rect.
        let body = self.standin_body_rect(conv.rect, conv.csd);
        // A live suspend (explicit action or suspend_on_close) is an explicit,
        // user-visible artifact, so it always returns on restore.
        let suspended = Rc::new(SuspendedWindow::new(
            sid,
            body.size,
            conv.identity,
            driftwm::session::Origin::Explicit,
            conv.csd,
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
        self.stage.set_position(window, body.loc);
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

/// The identity + geometry a conversion hands to the new stand-in.
pub struct SuspendConversion {
    pub identity: AppIdentity,
    pub rect: Rectangle<i32, Logical>,
    /// Whether the closing window was client-decorated. Every stand-in is
    /// barred, but a CSD origin shrinks the body under the bar (preserving the
    /// footprint) and reassembles the full geometry on adopt.
    pub csd: bool,
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
            driftwm::session::Origin::Explicit,
            false,
        ));
        self.map_window(StageWindow::Suspended(s), pos, true);
        sid
    }

    /// As [`Self::insert_suspended_for_test`], but a CSD-origin stand-in
    /// (`csd = true`): still barred chrome, but the origin flag drives adopt to
    /// hand the app back a full window (body height + bar) above `pos`.
    pub fn insert_suspended_csd_for_test(
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
            driftwm::session::Origin::Explicit,
            true,
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
        crate::render::ensure_body(&s, size, self.decoration_scale, &self.config.decorations);
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
