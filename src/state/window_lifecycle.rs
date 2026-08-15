//! The window mutation chokepoint: `map_window`, `raise_window`,
//! `unmap_window` and the stacking rules around them. Everything that changes
//! which windows exist or how they stack routes through here so stage
//! membership, the activation hint, and per-output leaves stay in step — the
//! `Space` clippy lint and `verify_stage_invariants` exist to catch anything
//! that goes around it.
//!
//! Also the per-surface teardown sweep, shared by the normal and crash
//! shutdown paths so the two cannot drift apart and leak.

use smithay::desktop::Window;
use smithay::reexports::wayland_server::Resource;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Logical, Point};
use smithay::wayland::seat::WaylandFocus;

use super::{DriftWm, RevealCause, StageWindow};

impl DriftWm {
    /// Push any `below` windows to the bottom of the z-order.
    /// Called after every raise to maintain stacking.
    pub fn enforce_below_windows(&mut self) {
        self.render.blur_geometry_generation += 1;
        self.stage.enforce_stacking();
    }

    /// Raise `window`, then its child windows, so a child/modal dialog stays
    /// directly above its own parent without jumping over unrelated windows
    /// that sit higher in the stack.
    pub fn raise_with_children(&mut self, window: &StageWindow) {
        // The stage does the raising and returns the raise order; activation is
        // exclusive to the topmost of it (the order's last element). Toggling it
        // per raised window instead would ping-pong the hint and flush a burst
        // of configures between a parent and its modal child. A stand-in target
        // no-ops its own `set_activated`, but exclusivity still clears the
        // previously-active client's hint — so focusing a stand-in deactivates
        // the client that had focus.
        if let Some(top) = self.stage.raise_with_children(window).last().cloned() {
            self.set_activated_exclusive(&top);
        }
    }

    /// Map (or move) `window` at `pos`, exclusively activating it if
    /// `activate` is set.
    pub fn map_window(
        &mut self,
        window: impl Into<StageWindow>,
        pos: Point<i32, Logical>,
        activate: bool,
    ) {
        let window = window.into();
        self.stage.map(window.clone(), pos);
        if activate {
            self.set_activated_exclusive(&window);
        }
    }

    /// The one z-raise with no deferred-adopt gate of its own. Its callers each
    /// establish that separately — one runs before a window can be stashed at
    /// all, the other only on a path a queued geometry request has already
    /// intercepted — so this offers a new caller no protection to inherit.
    pub fn raise_window(&mut self, window: &Window, activate: bool) {
        self.stage.raise(window);
        if activate {
            self.set_activated_exclusive(window);
        }
    }

    /// Remove `window` from the stage and send its per-output leaves. The stage
    /// side also purges it from the focus history (clamping any active cycle)
    /// and from the stage's fullscreen membership. The `fullscreen` viewport
    /// half (camera restore) is NOT handled here — a caller unmapping a
    /// fullscreen window must tear that down first, as `toplevel_destroyed` does.
    pub fn unmap_window(&mut self, window: &Window) {
        // Belt and braces: the dead-id sweep in `refresh_and_flush_clients` also
        // covers this, but drop eagerly so a re-map can't briefly resolve a
        // stale animation entry.
        if let Some(id) = self.stage.id_of(window) {
            self.window_animations.remove(id);
            self.drop_resize_crossfade(id);
        }
        self.stage.remove(window);
        super::membership::send_output_leaves(window);
    }

    /// Drop every per-surface map/cache entry keyed by `surface`. Shared by the
    /// normal and crash shutdown paths so the two can't drift apart and leak.
    /// Removal only, apart from the deferred-adopt reveal below — focus /
    /// fullscreen recovery stays at the call sites. Safe on non-toplevel
    /// surfaces: the extra lookups just miss.
    pub fn cleanup_surface_state(&mut self, surface: &WlSurface) {
        let id = surface.id();
        self.decorations
            .remove(&crate::decorations::DecorationKey::Surface(id.clone()));
        self.pending_ssd.remove(&id);
        self.pending_recenter.remove(&id);
        self.stable_snap_rects.remove(&id);
        self.pending_adopt_settle.remove(&id);
        self.pending_resizes.remove(&id);
        // `resolve_suspend_conversion` consumes the unmap snapshot on the normal
        // destroy path; drop it here too so a surface torn down through a path
        // that never reached that consume (the wl_surface-level cleanup safety
        // net) can't strand a snapshot past its surface.
        self.unmap_snapshots.remove(&id);
        // Captured close pixels are consumed at teardown; drop any that outlive
        // their surface (never-closed hide-to-tray captures, skipped animations).
        self.close_pixels.remove(&id);
        self.pending_center.remove(surface);
        self.pending_size.remove(surface);
        self.pending_fit.remove(surface);
        self.pending_fullscreen.remove(surface);
        // blur_cache is keyed per output, so drop every output's entry for this surface.
        self.render.blur_cache.retain(|(_, sid), _| sid != &id);
        self.render
            .shadow_cache
            .remove(&crate::decorations::DecorationKey::Surface(id.clone()));
        self.render
            .border_cache
            .remove(&crate::decorations::DecorationKey::Surface(id.clone()));
        // capture_state keys this surface's texture/damage tracker under "cap-tl:".
        self.render
            .capture_state
            .remove(&format!("cap-tl:{:?}", id));
        self.image_copy_capture_state.remove_toplevel(surface);
        // A relaunched surface that died before adoption — waiting for its first
        // commit, or for the grab holding the adopt back — must not leave either
        // stash behind (the pending relaunch itself is keyed by suspended id and
        // GC'd on its own deadline).
        self.pending_adoptions.remove(surface);
        // Drained through the reveal like every other exit from the stash, so no
        // path can leave a window hidden for an adopt that will never come. It
        // runs *after* the removals above and would write some of them back —
        // the reveal's own `alive()` guard is what stops that, not the call
        // order: the crash route gets here with the window still on the stage,
        // and only its dead surface turns the reveal into a plain drain.
        if let Some(idx) = self
            .deferred_adoptions
            .iter()
            .position(|d| d.root == *surface)
        {
            let entry = self.deferred_adoptions.remove(idx);
            self.reveal_deferred_adopt(&entry.root, entry.origin, RevealCause::Abandoned);
        }
        self.auto_anchor_snapshot.remove(surface);
        // Drop snapshots pointing at the destroyed surface as their anchor.
        // Keep `None`-anchor entries (user had no focus) and stand-in anchors
        // (no surface — never the destroyed one).
        self.auto_anchor_snapshot
            .retain(|_, anchor| match anchor.as_ref() {
                None => true,
                Some(w) => w.wl_surface().is_none_or(|s| &*s != surface),
            });
    }

    pub fn window_for_surface(&self, surface: &WlSurface) -> Option<Window> {
        self.stage
            .windows()
            .find(|w| w.wl_surface().as_deref() == Some(surface))
            .and_then(|w| w.client())
            .cloned()
    }
}
