//! The xdg Activated hint: exactly one window carries it, and a change reaches
//! the client even when no other configure follows.
//!
//! The subtlety is *when* to flush. A window that already has a configure
//! batched (first commit, fullscreen) should ride it — forcing the hint out
//! early splits that batch — hence the `set_activated_exclusive` /
//! `activate_riding_batch` pair over one shared implementation.

use super::{DriftWm, StageWindow};

impl DriftWm {
    /// Replicates `Space`'s activate semantics: xdg Activated set on `target`,
    /// cleared on every other window, and delivered on the wire for any window
    /// whose hint actually changed — so a focus change (click, raise) that
    /// isn't followed by another configure still reaches the client. Idempotent:
    /// a repeat call (hover, re-raise) changes nothing and sends nothing.
    pub(crate) fn set_activated_exclusive<Q>(&self, target: &Q)
    where
        StageWindow: PartialEq<Q>,
    {
        self.activate_exclusive(target, true);
    }

    /// Like `set_activated_exclusive`, but for a `target` that is about to
    /// receive a configure anyway — its batched first-commit or fullscreen
    /// send. Staging the target's hint lets it ride that configure instead of a
    /// premature standalone one; only the deactivated peers, which have no other
    /// configure coming, are flushed here.
    pub(crate) fn activate_riding_batch<Q>(&self, target: &Q)
    where
        StageWindow: PartialEq<Q>,
    {
        self.activate_exclusive(target, false);
    }

    /// Set xdg Activated on `target`, clear it elsewhere, and flush the hint for
    /// windows whose state changed and already had their initial configure sent
    /// — flushing a still-pending toplevel would force that configure out early,
    /// splitting the batched first-commit send. `flush_target` is false when a
    /// following send will carry the target's hint itself. Stand-ins never
    /// activate (`set_activated` no-ops, no toplevel), so they stay quiet.
    fn activate_exclusive<Q>(&self, target: &Q, flush_target: bool)
    where
        StageWindow: PartialEq<Q>,
    {
        for w in self.stage.windows() {
            if !w.set_activated(w == target) {
                continue;
            }
            if w == target && !flush_target {
                continue;
            }
            if let Some(toplevel) = w.toplevel()
                && toplevel.is_initial_configure_sent()
            {
                toplevel.send_pending_configure();
            }
        }
    }
}
