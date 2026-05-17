use std::sync::{Arc, Mutex};

use smithay::delegate_background_effect;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Logical, Rectangle};
use smithay::wayland::background_effect::{
    self, BackgroundEffectSurfaceCachedState, ExtBackgroundEffectHandler,
};
use smithay::wayland::compositor::{
    add_post_commit_hook, with_states, RegionAttributes, SurfaceData,
};

use crate::region::region_to_non_overlapping_rects;
use crate::state::DriftWm;

#[derive(Default)]
struct CachedBlurRegionUserData(Mutex<CachedBlurRegionInner>);

#[derive(Default)]
struct CachedBlurRegionInner {
    pending_dirty: bool,
    dirty: bool,
    hook_registered: bool,
    /// Non-overlapping rects in surface-local logical coords. `None` = no
    /// region set (or unset). Empty `Vec` = explicit empty region (client
    /// opted out of blur).
    rects: Option<Arc<Vec<Rectangle<i32, Logical>>>>,
}

/// Get the cached blur region for a surface. Lazily recomputes when dirty.
///
/// Limitation: this is per-surface. The render path uses the toplevel
/// wl_surface only, so blur regions on subsurfaces are silently ignored —
/// matches driftwm's whole-surface-tree alpha-mask flow today. Most expected
/// clients (mako, swaync, custom shells) use a single toplevel.
pub fn get_cached_blur_region(states: &SurfaceData) -> Option<Arc<Vec<Rectangle<i32, Logical>>>> {
    let cache = states
        .data_map
        .get_or_insert_threadsafe(CachedBlurRegionUserData::default);
    let mut guard = cache.0.lock().unwrap();

    if guard.dirty {
        guard.dirty = false;
        recompute_blur_region(states, &mut guard);
    }

    guard.rects.clone()
}

fn recompute_blur_region(states: &SurfaceData, inner: &mut CachedBlurRegionInner) {
    let cached = &states.cached_state;

    let rects = if let Some(arc) = &mut inner.rects {
        arc
    } else {
        inner.rects.insert(Arc::new(Vec::new()))
    };
    let rects = Arc::make_mut(rects);

    if cached.has::<BackgroundEffectSurfaceCachedState>() {
        let mut guard = cached.get::<BackgroundEffectSurfaceCachedState>();
        if let Some(region) = &guard.current().blur_region {
            region_to_non_overlapping_rects(region, rects);
        } else {
            inner.rects = None;
        }
        return;
    }

    inner.rects = None;
}

fn mark_blur_region_pending_dirty(wl_surface: &WlSurface) {
    let register_hook = with_states(wl_surface, |states| {
        let cache = states
            .data_map
            .get_or_insert_threadsafe(CachedBlurRegionUserData::default);
        let mut guard = cache.0.lock().unwrap();
        guard.pending_dirty = true;

        if guard.hook_registered {
            false
        } else {
            guard.hook_registered = true;
            true
        }
    });

    if register_hook {
        add_post_commit_hook::<DriftWm, _>(wl_surface, |state, _dh, surface| {
            let became_dirty = with_states(surface, |states| {
                let Some(cache) = states.data_map.get::<CachedBlurRegionUserData>() else {
                    tracing::error!("unexpected missing CachedBlurRegionUserData");
                    return false;
                };
                let mut guard = cache.0.lock().unwrap();
                if guard.pending_dirty {
                    guard.pending_dirty = false;
                    guard.dirty = true;
                    true
                } else {
                    false
                }
            });

            // The region-only commit doesn't damage the surface's drawn
            // content — we have to ask for a redraw explicitly, otherwise
            // a still surface that swaps its blur rect won't repaint.
            if became_dirty {
                state.mark_dirty_for_surface(surface);
            }
        });
    }
}

impl ExtBackgroundEffectHandler for DriftWm {
    fn capabilities(&self) -> background_effect::Capability {
        background_effect::Capability::Blur
    }

    fn set_blur_region(&mut self, wl_surface: WlSurface, _region: RegionAttributes) {
        mark_blur_region_pending_dirty(&wl_surface);
    }

    fn unset_blur_region(&mut self, wl_surface: WlSurface) {
        mark_blur_region_pending_dirty(&wl_surface);
    }
}

delegate_background_effect!(DriftWm);
