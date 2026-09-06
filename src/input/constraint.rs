//! The one place the crate opens a pointer-constraint closure.
//!
//! `with_pointer_constraint` runs its closure while the surface's user-data
//! mutex is held, and that mutex is not re-entrant: reading window geometry or
//! opening `with_states` on the same surface from inside it hangs the
//! compositor with no panic and no log. So the closures here only copy fields
//! out or flip the constraint's flag, and every caller works from plain data
//! with the lock already released.
//!
//! `clippy.toml` bans `with_pointer_constraint` everywhere else. That ban sees
//! call sites, not the function taken as a value, so it is a guard rail rather
//! than a proof.

use smithay::input::pointer::PointerHandle;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::wayland::compositor::RegionAttributes;
use smithay::wayland::pointer_constraints::{PointerConstraint, with_pointer_constraint};

use crate::state::DriftWm;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConstraintKind {
    Locked,
    Confined,
}

#[derive(Debug, Clone)]
pub(crate) struct ConstraintSnapshot {
    pub kind: ConstraintKind,
    pub active: bool,
    /// Cloned out of the constraint so callers test it after the lock is gone.
    pub region: Option<RegionAttributes>,
}

/// `None` when the surface carries no constraint for this pointer.
pub(crate) fn constraint_snapshot(
    surface: &WlSurface,
    pointer: &PointerHandle<DriftWm>,
) -> Option<ConstraintSnapshot> {
    #[allow(clippy::disallowed_methods)] // the sanctioned closure: it only copies fields out
    let snapshot = with_pointer_constraint(surface, pointer, |constraint| {
        let constraint = constraint?;
        Some(ConstraintSnapshot {
            kind: match &*constraint {
                PointerConstraint::Locked(_) => ConstraintKind::Locked,
                PointerConstraint::Confined(_) => ConstraintKind::Confined,
            },
            active: constraint.is_active(),
            region: constraint.region().cloned(),
        })
    });
    snapshot
}

/// No-op without a constraint; smithay's `activate` is idempotent.
pub(crate) fn activate_constraint(surface: &WlSurface, pointer: &PointerHandle<DriftWm>) {
    #[allow(clippy::disallowed_methods)] // the sanctioned closure: it only flips the flag
    with_pointer_constraint(surface, pointer, |constraint| {
        if let Some(constraint) = constraint {
            constraint.activate();
        }
    });
}

/// No-op without an active constraint; smithay's `deactivate` is idempotent
/// anyway.
pub(crate) fn deactivate_constraint(surface: &WlSurface, pointer: &PointerHandle<DriftWm>) {
    #[allow(clippy::disallowed_methods)] // the sanctioned closure: it only flips the flag
    with_pointer_constraint(surface, pointer, |constraint| {
        if let Some(constraint) = constraint
            && constraint.is_active()
        {
            constraint.deactivate();
        }
    });
}
