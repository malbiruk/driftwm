//! Keyboard-focus policy: who gets focus and what has to happen on the way
//! there — raise-and-focus, modal redirect, layer-shell interactivity, empty
//! canvas, and the suspended stand-in cases.
//!
//! `focus.rs` is the type half ([`FocusTarget`] and its smithay trait impls);
//! this is the decision half.

use smithay::desktop::{PopupUngrabStrategy, Window};
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::wayland::seat::WaylandFocus;

use driftwm::window_ext::WindowExt;

use super::{DriftWm, FocusIntent, FocusTarget, StageWindow, SuspendedId};

impl DriftWm {
    /// Innermost modal descendant for focus redirect. Chases modal chains
    /// (e.g. file picker → overwrite confirm); capped at 10 to guard against
    /// circular parents.
    pub fn topmost_modal_child(&self, window: &Window) -> Option<Window> {
        let parent_surface = window.wl_surface()?;
        let child = self
            .stage
            .windows()
            .rfind(|w| w.parent_surface().as_ref() == Some(&*parent_surface) && w.is_modal())
            .and_then(|w| w.client())
            .cloned()?;
        self.topmost_modal_child_inner(&child, 9).or(Some(child))
    }

    fn topmost_modal_child_inner(&self, window: &Window, depth: u8) -> Option<Window> {
        if depth == 0 {
            return None;
        }
        let parent_surface = window.wl_surface()?;
        let child = self
            .stage
            .windows()
            .rfind(|w| w.parent_surface().as_ref() == Some(&*parent_surface) && w.is_modal())
            .and_then(|w| w.client())
            .cloned()?;
        self.topmost_modal_child_inner(&child, depth - 1)
            .or(Some(child))
    }

    /// Raise a window and focus it (or its innermost modal child).
    pub fn raise_and_focus(&mut self, window: &Window, serial: smithay::utils::Serial) {
        // A window held back for a deferred adopt is not drawn, and the adopt is
        // about to hand it a different z-slot outright — so neither half of this
        // may land on it from any route. The reveal owes it both.
        if self.hidden_by_deferred_adopt(window) {
            return;
        }
        self.raise_with_children(&StageWindow::Client(window.clone()));
        self.enforce_below_windows();

        let focus_surface = self
            .topmost_modal_child(window)
            .or(Some(window.clone()))
            .and_then(|w| w.wl_surface().map(|s| FocusTarget(s.into_owned())));

        self.set_window_focus(focus_surface, serial);
    }

    /// Raise + focus a stage element — the element-generic form of
    /// `raise_and_focus` / `focus_and_raise_suspended`.
    pub fn raise_and_focus_element(
        &mut self,
        element: &StageWindow,
        serial: smithay::utils::Serial,
    ) {
        match element {
            StageWindow::Client(w) => self.raise_and_focus(w, serial),
            StageWindow::Suspended(s) => self.focus_and_raise_suspended(s.id),
        }
    }

    /// Write the window-focus intent, arming the durable write when it actually
    /// changes. Every production assignment of the field goes through here, so
    /// the two setters and `focus_changed`'s rewrite share one guard.
    ///
    /// The envelope records which entry held focus, and no other mark-dirty
    /// site is focus-driven: without this, "the window you had focused comes
    /// back focused" would degrade to as of the last window mutation. Hover
    /// focus arms as well, deliberately — `focus_follows_mouse` makes most
    /// focus changes hover changes, so skipping them would miss exactly the
    /// users the promise is for, and the debounce bounds a pointer crossing
    /// windows to one write per second.
    ///
    /// That bound needs the equality guard: the hover paths already refuse to
    /// re-focus what is focused, but `raise_and_focus` re-seats the same intent
    /// on every click.
    pub(crate) fn set_focus_intent(&mut self, intent: Option<FocusIntent>) {
        if self.window_focus == intent {
            return;
        }
        self.window_focus = intent;
        self.session_store_mark_dirty();
    }

    /// Record a window-level keyboard-focus intent and recompute the actual
    /// focus. Higher-priority owners (an exclusive / on-demand layer surface)
    /// still win — this is what keeps a launcher focused while the pointer
    /// moves over a window underneath it.
    pub fn set_window_focus(
        &mut self,
        target: Option<FocusTarget>,
        serial: smithay::utils::Serial,
    ) {
        // The keyboard may not land on a window nobody can see. The last gate
        // rather than the only one, so a caller that resolved past
        // `raise_and_focus` still can't seat focus on a hidden adopt.
        if let Some(t) = &target
            && self.root_hidden_by_deferred_adopt(&t.0)
        {
            return;
        }
        self.set_focus_intent(target.map(FocusIntent::Surface));
        // Unconditional, `None` included: only `clear_focus_to_empty_canvas` means
        // "blank slate" — a flag surviving an incidental clear would silently kill
        // the fallback for the rest of the session.
        self.suppress_auto_anchor = false;
        // An explicit window focus supersedes any on-demand layer focus.
        self.on_demand_layer = None;
        self.update_keyboard_focus(serial);
    }

    /// Clear focus because the user clicked bare canvas — a deliberate blank
    /// slate, distinct from the incidental focus loss `set_window_focus(None)`
    /// also expresses. A named entry point keeps the two from re-merging:
    /// without it, a dying surface or closing window would read as "nothing
    /// anchored".
    pub fn clear_focus_to_empty_canvas(&mut self, serial: smithay::utils::Serial) {
        self.set_window_focus(None, serial);
        // After, not before: the setter clears the flag.
        self.suppress_auto_anchor = true;
    }

    /// Focus a suspended window: record the intent and clear seat keyboard
    /// focus (a suspended window has no surface to hold it). Higher-priority
    /// owners (lock / exclusive-or-on-demand layer) still win via
    /// `update_keyboard_focus`, which is THE GATE for every suspended-focus
    /// behavior.
    pub fn set_suspended_focus(&mut self, id: SuspendedId, serial: smithay::utils::Serial) {
        self.set_focus_intent(Some(FocusIntent::Suspended(id)));
        self.suppress_auto_anchor = false;
        self.on_demand_layer = None;
        self.update_keyboard_focus(serial);
    }

    /// The surface-focus intent, if any (`None` while a suspended window is the
    /// intended focus).
    pub fn window_focus_surface(&self) -> Option<&FocusTarget> {
        match &self.window_focus {
            Some(FocusIntent::Surface(t)) => Some(t),
            _ => None,
        }
    }

    /// The focus *intent* resolved to a stage element, for auto-placement
    /// anchoring: a live window for a `Surface` intent, the stand-in for a
    /// `Suspended` one. Reads intent (not the derived seat focus) so it survives
    /// a launcher's transient keyboard focus, matching `window_focus_surface`.
    /// `None` when the user had no focused window (empty-canvas click) or the
    /// intended target is already gone.
    pub fn focused_anchor_element(&self) -> Option<StageWindow> {
        match self.window_focus.as_ref()? {
            FocusIntent::Surface(t) => self.window_for_surface(&t.0).map(StageWindow::Client),
            FocusIntent::Suspended(id) => self.find_suspended(*id).map(StageWindow::Suspended),
        }
    }

    /// The suspended window that currently holds focus *under THE GATE*: intent
    /// is `Suspended` AND no higher-priority owner holds the derived seat
    /// keyboard focus (lock / exclusive-or-on-demand layer / keyboard grab all
    /// surface as a non-`None` seat focus). Intent alone is not authority.
    pub fn gated_suspended_focus(&self) -> Option<SuspendedId> {
        let FocusIntent::Suspended(id) = self.window_focus.as_ref()? else {
            return None;
        };
        let seat_focus_empty = self
            .seat
            .get_keyboard()
            .is_none_or(|kb| kb.current_focus().is_none());
        seat_focus_empty.then_some(*id)
    }

    /// Derive and apply the authoritative keyboard focus from the current
    /// state, in priority order: session lock (handled imperatively, so we
    /// bail) → exclusive layer → on-demand layer → focused window.
    pub fn update_keyboard_focus(&mut self, serial: smithay::utils::Serial) {
        if self.session_lock.is_locked() {
            return;
        }

        let target = self
            .exclusive_layer_focus()
            .or_else(|| self.on_demand_layer_focus())
            .or_else(|| self.focused_window_target());

        // Focus left the grab root: tear the stale grab down ourselves (see PopupGrabState).
        let leaving_grab_root = self
            .popup_grab
            .as_ref()
            .is_some_and(|g| g.has_keyboard_grab && target.as_ref().map(|t| &t.0) != Some(&g.root));
        if leaving_grab_root && let Some(mut g) = self.popup_grab.take() {
            g.grab.ungrab(PopupUngrabStrategy::All);
            let time = self.start_time.elapsed().as_millis() as u32;
            self.seat.get_keyboard().unwrap().unset_grab(self);
            // Defer the pointer ungrab to an idle: a focus change can originate
            // inside a PointerGrab's own callback (PanGrab's click-on-empty-canvas
            // moves focus from its button handler), and PointerHandle holds a
            // non-reentrant mutex across that callback. Calling `unset_grab` inline
            // would re-lock it on the same thread and hang the compositor; the idle
            // runs once dispatch unwinds and the lock is free. Whatever owns the
            // pointer by then — the popup grab on the keyboard path or the drag
            // grab itself on the mouse path — has finished interacting, so ending
            // it is harmless.
            self.loop_handle.insert_idle(move |data| {
                data.seat
                    .get_pointer()
                    .unwrap()
                    .unset_grab(data, serial, time);
            });
        }

        // Focus staying on the grab root: a live grab keeps ownership (it rejects
        // the change), and an ended-but-still-attached grab releases on this
        // set_focus. Route through the keyboard directly, skipping the per-window
        // layout swap the live grab would otherwise trigger spuriously.
        let keyboard = self.seat.get_keyboard().unwrap();
        if keyboard.is_grabbed() {
            keyboard.set_focus(self, target, serial);
            return;
        }

        self.set_keyboard_focus(target, serial);
    }

    /// The window the keyboard falls back to when no layer owns focus. Prefers
    /// the live `window_focus` intent; if that window died while a layer or
    /// lock held focus, recovers via the most-recent live history entry rather
    /// than focusing nothing. A deliberate `None` (e.g. click on empty canvas)
    /// stays `None`.
    fn focused_window_target(&self) -> Option<FocusTarget> {
        use smithay::utils::IsAlive;
        match &self.window_focus {
            // A suspended window holds no seat keyboard focus.
            Some(FocusIntent::Suspended(_)) => None,
            Some(FocusIntent::Surface(t)) if t.0.alive() => Some(t.clone()),
            Some(FocusIntent::Surface(_)) => self
                .stage
                .focus_history()
                .iter()
                .find(|w| w.alive())
                .and_then(|w| w.wl_surface().map(|s| FocusTarget(s.into_owned()))),
            None => None,
        }
    }

    /// First mapped layer surface (across outputs and canvas layers) that
    /// requests `Exclusive` keyboard interactivity, in z-priority order.
    fn exclusive_layer_focus(&self) -> Option<FocusTarget> {
        use smithay::utils::IsAlive;
        use smithay::wayland::shell::wlr_layer::{KeyboardInteractivity, Layer};

        for idx in self.canvas_layer_indices_sorted() {
            let cl = &self.canvas_layers[idx];
            let s = cl.surface.wl_surface();
            if s.alive()
                && cl.surface.cached_state().keyboard_interactivity
                    == KeyboardInteractivity::Exclusive
            {
                return Some(FocusTarget(s.clone()));
            }
        }
        for output in self.space.outputs() {
            for layer in [Layer::Overlay, Layer::Top, Layer::Bottom, Layer::Background] {
                for (surface, _) in self.layers_on_sorted(output, layer) {
                    let s = surface.wl_surface();
                    // A client tearing down several layers destroys them one at a
                    // time, and this recompute runs from each `layer_destroyed` —
                    // so a sibling that is already dead can still be mapped here.
                    if surface.alive()
                        && s.alive()
                        && surface.cached_state().keyboard_interactivity
                            == KeyboardInteractivity::Exclusive
                    {
                        return Some(FocusTarget(s.clone()));
                    }
                }
            }
        }
        None
    }

    /// The tracked on-demand layer surface, if it's still mapped and still
    /// requests `OnDemand` interactivity.
    fn on_demand_layer_focus(&self) -> Option<FocusTarget> {
        use smithay::utils::IsAlive;
        use smithay::wayland::shell::wlr_layer::KeyboardInteractivity;

        let surface = self.on_demand_layer.as_ref()?;
        if !surface.alive() {
            return None;
        }
        (self.layer_interactivity(surface) == Some(KeyboardInteractivity::OnDemand))
            .then(|| FocusTarget(surface.clone()))
    }

    /// On a click over a layer surface, grant it keyboard focus if it requests
    /// `OnDemand`. A click elsewhere (passed `None` or a non-on-demand layer)
    /// clears any existing on-demand focus.
    pub fn focus_layer_if_on_demand(
        &mut self,
        surface: Option<WlSurface>,
        serial: smithay::utils::Serial,
    ) {
        use smithay::wayland::compositor::get_parent;
        use smithay::wayland::shell::wlr_layer::KeyboardInteractivity;

        // The pointer's focus may be a subsurface; resolve to the root surface
        // that the layer is keyed by.
        let surface = surface.map(|mut s| {
            while let Some(parent) = get_parent(&s) {
                s = parent;
            }
            s
        });

        if let Some(surface) = surface
            && self.layer_interactivity(&surface) == Some(KeyboardInteractivity::OnDemand)
        {
            if self.on_demand_layer.as_ref() != Some(&surface) {
                self.on_demand_layer = Some(surface);
                self.update_keyboard_focus(serial);
            }
            return;
        }

        if self.on_demand_layer.take().is_some() {
            self.update_keyboard_focus(serial);
        }
    }

    /// Single point where keyboard focus is applied.
    pub fn set_keyboard_focus(
        &mut self,
        target: Option<FocusTarget>,
        serial: smithay::utils::Serial,
    ) {
        let keyboard = self.seat.get_keyboard().unwrap();

        if self.config.remember_layout_per_window {
            let old = keyboard.current_focus();
            let focus_changing = old.as_ref().map(|f| &f.0) != target.as_ref().map(|f| &f.0);
            if focus_changing {
                self.remember_window_layout(&keyboard, old.as_ref(), target.as_ref());
            }
        }

        keyboard.set_focus(self, target, serial);
    }

    /// Save the active layout on the outgoing window, restore the incoming one's.
    /// Unfocuses before swapping so the outgoing client never sees the layout change.
    fn remember_window_layout(
        &mut self,
        keyboard: &smithay::input::keyboard::KeyboardHandle<Self>,
        old: Option<&FocusTarget>,
        new: Option<&FocusTarget>,
    ) {
        use smithay::input::keyboard::Layout;
        use smithay::utils::IsAlive;
        use smithay::wayland::compositor::with_states;
        use std::cell::Cell;

        let current =
            keyboard.with_xkb_state(self, |ctx| ctx.xkb().lock().unwrap().active_layout());

        if let Some(old) = old
            && old.0.alive()
        {
            with_states(&old.0, |states| {
                states
                    .data_map
                    .get_or_insert::<Cell<Layout>, _>(Cell::default)
                    .set(current)
            });
        }

        let Some(new) = new else { return };
        let saved = with_states(&new.0, |states| {
            states
                .data_map
                .get_or_insert::<Cell<Layout>, _>(Cell::default)
                .get()
        });

        let layout_count =
            keyboard.with_xkb_state(self, |ctx| ctx.xkb().lock().unwrap().layouts().count());
        if saved == current || saved.0 as usize >= layout_count {
            return;
        }

        keyboard.set_focus(self, None, smithay::utils::SERIAL_COUNTER.next_serial());
        let name = keyboard.with_xkb_state(self, |mut ctx| {
            ctx.set_layout(saved);
            let xkb = ctx.xkb().lock().unwrap();
            xkb.layout_name(xkb.active_layout()).to_owned()
        });
        self.active_layout = name;
    }

    /// Keyboard-focused window, matched on the toplevel's own surface — see
    /// [`Self::focus_root_window`] for the window a focused popup belongs to.
    /// Does not filter widgets — pair with `.filter(|w| !w.is_widget())` if
    /// needed.
    pub fn focused_window(&self) -> Option<Window> {
        let keyboard = self.seat.get_keyboard()?;
        let focus = keyboard.current_focus()?;
        self.stage
            .windows()
            .find(|w| w.wl_surface().as_deref() == Some(&focus.0))
            .and_then(|w| w.client())
            .cloned()
    }

    /// The window whose picture reads as focused: the keyboard-focused window, or
    /// the one owning the keyboard-focused popup — a popup grab moves the seat's
    /// focus onto the popup surface at the first key event.
    pub(crate) fn focus_root_window(&self) -> Option<Window> {
        let keyboard = self.seat.get_keyboard()?;
        let focus = keyboard.current_focus()?;
        self.window_for_surface_root(&focus.0)
    }

    /// The element action dispatch should treat as focused: the keyboard-focused
    /// client window, else the stand-in holding gated suspended focus. The two
    /// sources are mutually exclusive (a stand-in holds no seat keyboard focus),
    /// so the client-first order changes no reachable outcome. Contrast
    /// `focused_anchor_element`, which reads raw focus *intent* without the gate.
    pub fn focused_element(&self) -> Option<StageWindow> {
        if let Some(window) = self.focused_window() {
            return Some(StageWindow::Client(window));
        }
        self.gated_suspended_focus()
            .and_then(|id| self.find_suspended(id))
            .map(StageWindow::Suspended)
    }
}
