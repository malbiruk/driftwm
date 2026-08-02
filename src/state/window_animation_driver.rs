//! Compositor-side driver for window animations: starting, cancelling, and
//! ticking entries, resolving them against client commits, and answering the
//! render loop's per-frame questions — animated visual, chrome alpha, cull
//! rect, fullscreen cover.
//!
//! [`super::window_animation`] holds the smithay-free state machine; this is
//! everything that needs `DriftWm` (config, stage, per-output camera) to feed
//! it. The split is a file boundary, not encapsulation: `WindowAnimations`'
//! own methods stay `pub(crate)` for `fit.rs`, `suspended.rs`, the winit
//! backend, and the tests.

use std::time::{Duration, Instant};

use smithay::desktop::Window;
use smithay::reexports::wayland_server::Resource;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Logical, Point, Rectangle, Size};

use driftwm::stage::{ElementId, StageElement};
use smithay::wayland::compositor::{BufferAssignment, SurfaceAttributes, with_states};
use smithay::wayland::seat::WaylandFocus;

use smithay::output::Output;

use super::window_animation::{
    AnimSpace, AnimatedVisual, ContentPolicy, FrozenPicture, FullscreenCover, GeometryRole,
    MIN_ANIMATED_RESIZE,
};
use super::{DriftWm, PendingView, StageWindow, output_logical_size, output_state};

impl DriftWm {
    /// Render-time animated stand-in for the window with stable id `id`, given
    /// its live target rect (canvas rect for a normal window, screen rect for a
    /// pinned one). Identity when nothing is animating.
    pub(crate) fn animated_visual(
        &self,
        id: ElementId,
        target_loc: Point<f64, Logical>,
        target_size: Size<f64, Logical>,
    ) -> AnimatedVisual {
        self.window_animations.animated_visual(
            id,
            target_loc,
            target_size,
            self.config.effects.animation_scale,
        )
    }

    /// How opaque the compositor chrome around `window` is. Stage fullscreen
    /// membership answers this as a hard on/off, but a geometry leg crosses
    /// between two pictures: it starts on the one its freeze held and ends on
    /// whatever the live window wears, so the chrome fades between them instead
    /// of popping. `id` is passed in because the render loop resolves it once per
    /// window per output.
    pub(crate) fn chrome_alpha_of(&self, id: Option<ElementId>, window: &Window) -> f32 {
        let live = if self.stage.is_fullscreen(window) {
            0.0
        } else {
            1.0
        };
        id.and_then(|id| self.window_animations.chrome_ramp(id))
            .map_or(live, |(from, travelled)| {
                super::window_animation::chrome_alpha(from, live, travelled)
            })
    }

    /// Whether the picture on screen for `window` wears no compositor chrome at
    /// all — the settled fullscreen look, or a freeze still holding it.
    pub(crate) fn chrome_fullscreen(&self, window: &Window) -> bool {
        self.chrome_alpha_of(self.stage.id_of(window), window) <= 0.0
    }

    /// Whether the picture on screen for `window` is a screen-pinned one.
    /// Entering fullscreen unpins at the action, so the live answer would restack
    /// a frozen pre-action frame out of the pinned bucket while it sits still.
    pub(crate) fn pinned_picture_of(&self, id: Option<ElementId>, window: &Window) -> bool {
        id.and_then(|id| self.window_animations.frozen_pinned(id))
            .unwrap_or_else(|| self.is_pinned(window))
    }

    /// Whether that picture takes the screen-pinned z-bucket, which draws above
    /// every normal window. It does not on an output a fullscreen picture is
    /// covering (`output_fullscreen`, passed in because the composer resolves it
    /// once per output): the only windows drawn there are fullscreen pictures,
    /// and the bucket would invert them — an exit that re-pinned on its way out
    /// would draw over the very window taking its fullscreen over, for the whole
    /// handover. Sharing the normal bucket leaves the stage's z-order to decide,
    /// which a fullscreen picture never loses: widgets — the one other bucket a
    /// window can land in — never fullscreen and are never a cover. The pin
    /// *marker* still follows [`Self::pinned_picture_of`], and a covering picture
    /// wears no title bar to put it on anyway.
    pub(crate) fn draws_pinned_on(
        &self,
        id: Option<ElementId>,
        window: &Window,
        output_fullscreen: bool,
    ) -> bool {
        !output_fullscreen && self.pinned_picture_of(id, window)
    }

    /// True if a canvas rect intersects some output that can actually draw it
    /// (live, not DPMS-off). Animations intersecting no such output complete
    /// instantly, so they never wedge the udev idle fast-path.
    pub(crate) fn canvas_rect_drawable(&self, rect: Rectangle<i32, Logical>) -> bool {
        self.space.outputs().any(|o| {
            if self.dpms_off_outputs.contains(o) {
                return false;
            }
            // Judging against the live camera instead of `world_view` would
            // instant-complete an animation in the band still shown behind a
            // growing fullscreen entry.
            let (camera, zoom) = self.world_view(o);
            let viewport = super::output_logical_size(o);
            driftwm::canvas::visible_canvas_rect(camera.to_i32_round(), viewport, zoom)
                .overlaps(rect)
        })
    }

    fn output_name_drawable(&self, name: &str) -> bool {
        self.space
            .outputs()
            .any(|o| o.name() == name && !self.dpms_off_outputs.contains(o))
    }

    /// Start the window-open scale+fade. No-op under an interactive grab or when
    /// the window's rect intersects no drawable output (instant-complete).
    pub(crate) fn start_window_open_animation(&mut self, window: &Window) {
        let Some(id) = self.stage.id_of(window) else {
            return;
        };
        if self.element_under_interactive_grab(&StageWindow::Client(window.clone())) {
            return;
        }
        let loc = self.stage.position_of(window).unwrap_or_default();
        if !self.canvas_rect_drawable(Rectangle::new(loc, StageElement::size(window))) {
            return;
        }
        // An open entry overwrites whatever the id was doing — a hide-to-tray app
        // can remap mid-resize — so the crossfade halves of the geometry entry it
        // replaces go with it, rather than fading over the opening window.
        self.drop_resize_crossfade(id);
        self.window_animations.start_open(id);
    }

    /// Drop everything staged for `window`'s animation — the entry itself and
    /// both halves of its resize crossfade — leaving it drawn at its live
    /// geometry, with no freeze, no leg and nothing fading over it.
    ///
    /// This is how an action makes a geometry change instant: apply the change,
    /// then take down what it armed. A change that lands whole in one frame has
    /// nothing for a leg to travel over — it would only chase a scene the
    /// camera, or the window's output, has already left.
    pub(crate) fn cancel_window_animation(&mut self, window: &Window) {
        let Some(id) = self.stage.id_of(window) else {
            return;
        };
        self.window_animations.remove(id);
        self.drop_resize_crossfade(id);
    }

    /// End `element`'s animation because something else — a drag — has taken
    /// control of its geometry, landing anything the entry still owed.
    ///
    /// Unlike [`Self::cancel_window_animation`], which discards a parked camera
    /// move along with the entry, this applies it: a drag that interrupts a fit
    /// still owes the viewport the pan that fit arranged, and the entry it takes
    /// down is the only thing that could ever have handed it back.
    pub(crate) fn end_element_animation(&mut self, element: &StageWindow) {
        let Some(id) = self.stage.id_of(element) else {
            return;
        };
        if let Some(pending) = self.window_animations.take_pending_view(id) {
            self.apply_pending_view(pending);
        }
        self.window_animations.remove(id);
        self.drop_resize_crossfade(id);
    }

    /// Shared start path for every geometry chase: resolve the id, honor the
    /// interactive-grab guard, instant-complete (skip) when the seed rect
    /// intersects no drawable output, else (re)start the chase. `replace_visual`
    /// forces the seed onto an existing entry — the seeded (fullscreen) callers
    /// convert coordinate frames, so keeping the old visual would jump at zoom≠1.
    ///
    /// `final_loc` is where this chase ends up, and only the caller knows it:
    /// a fullscreen enter maps before arming while a fit arms before mapping, so
    /// deriving it from the stage would be right for one and the placement rect
    /// for the other. Supplied only by the two callers that can run inside a
    /// window's map commit, where it seeds an open fade at the destination.
    #[allow(clippy::too_many_arguments)]
    fn start_geometry_entry(
        &mut self,
        element: &StageWindow,
        seed: Rectangle<f64, Logical>,
        space: AnimSpace,
        requested_size: Option<Size<i32, Logical>>,
        role: GeometryRole,
        replace_visual: bool,
        content_policy: ContentPolicy,
        waits_for: Option<ElementId>,
        final_loc: Option<Point<i32, Logical>>,
    ) {
        let Some(id) = self.stage.id_of(element) else {
            return;
        };
        if self.element_under_interactive_grab(element) {
            return;
        }
        // A window that acquires an action of its own stops waiting to be pushed
        // by anyone else's. Read from what the caller asked for, before the
        // threshold below can drop it: a resize too small to freeze for is still
        // this window's own action.
        let carries_request = requested_size.is_some();
        let committed = element.geometry().size;
        // A request the window cannot visibly answer is no request at all: drop
        // it here, once, so the freeze `start_geometry` arms and the capture
        // dropped below can never disagree about whether a resize is starting.
        // Resolved before the open-fade override below, which sizes its seed
        // from whatever request survives this.
        let requested_size = requested_size.filter(|size| {
            (size.w - committed.w)
                .abs()
                .max((size.h - committed.h).abs())
                > MIN_ANIMATED_RESIZE
        });
        // A chase that replaces an open entry inherits its fade rather than
        // destroying it, so a window still arriving keeps arriving instead of
        // popping to full opacity partway in. Only the *seed* is overridden, and
        // only while that fade has never been drawn: a window already shown at
        // its placement rect has to travel from there, but one that has not is
        // put straight at the rect it is going to reach — a zero-length chase,
        // so it fades in already fullscreen (or already fitted). The seed's size
        // comes from the surviving request, so it is exactly what the tick
        // converges to.
        let open_fade = self.window_animations.open_progress(id);
        let open_unshown = self.window_animations.open_unshown(id);
        let seed = match final_loc {
            Some(loc) if open_unshown => {
                Rectangle::new(loc.to_f64(), requested_size.unwrap_or(committed).to_f64())
            }
            _ => seed,
        };
        let eligible = match &space {
            AnimSpace::Screen(name) => self.output_name_drawable(name),
            AnimSpace::Canvas => self.canvas_rect_drawable(seed.to_i32_round()),
        };
        if !eligible {
            return;
        }
        // A brand new resize supersedes the last one: its captured content is for
        // a request nobody waits on any more, and a live overlay belongs to a leg
        // that no longer exists.
        if requested_size.is_some() {
            self.drop_resize_crossfade(id);
        }
        // What the picture this leg starts from looked like — see
        // [`GeometryRole`] and [`FrozenPicture`].
        let fullscreen_output = match &role {
            GeometryRole::FullscreenEntry { .. } => None,
            GeometryRole::FullscreenExit { output } => self.output_by_name(output),
            // A stand-in has no surface and is never fullscreen, so this is
            // `None` for one — which is the right answer, not a shortcut.
            GeometryRole::Normal => element
                .wl_surface()
                .and_then(|s| self.find_fullscreen_output_for_surface(&s)),
        };
        let picture = FrozenPicture {
            // A picture drawn translucent cannot claim to cover its output: the
            // cull behind a fullscreen cover would hide the scene while the
            // window said to be hiding it is still see-through. It is still a
            // fullscreen picture, and `bare` below says so.
            fullscreen_on: fullscreen_output
                .clone()
                .filter(|_| open_fade.is_none())
                .map(|o| {
                    // A re-pinned exit draws in screen space and covers the output
                    // wherever the canvas goes (see `FullscreenCover::view`) —
                    // stamping a view on it would uncover the scene under a picture
                    // that is still hiding it, and pop the layer bar back over a
                    // motionless fullscreen frame.
                    let view = matches!(space, AnimSpace::Canvas).then(|| {
                        let os = output_state(&o);
                        // The exit restores the camera before arming this, so the live
                        // view is the one the seed was converted into.
                        (os.camera, os.zoom)
                    });
                    FullscreenCover {
                        output: o.name(),
                        view,
                    }
                }),
            bare: fullscreen_output.is_some(),
            pinned: match &role {
                GeometryRole::FullscreenEntry { was_pinned } => *was_pinned,
                // Every other leg — the exit's re-pin included — is armed after
                // whatever pin change it rides, so live describes the picture.
                _ => self.is_pinned(element),
            },
            undrawn: open_unshown,
        };
        if carries_request {
            self.window_animations.clear_waits_for(id);
        }
        self.window_animations.start_geometry(
            id,
            seed,
            space,
            requested_size,
            committed,
            role,
            replace_visual,
            content_policy,
            picture,
            waits_for,
            open_fade,
        );
    }

    /// The rect `window` is drawn at on `output` right now, in that output's
    /// screen px. Prefers an in-flight geometry entry's visual, so an
    /// interrupted transition stays continuous, and reads it in *the entry's*
    /// own space rather than the one live pin membership implies — the two can
    /// disagree, and taking canvas coords for screen px is not a near miss.
    pub(crate) fn window_screen_rect_on(
        &self,
        window: &Window,
        output: &Output,
    ) -> Option<Rectangle<f64, Logical>> {
        let (camera, zoom) = {
            let os = output_state(output);
            (os.camera, os.zoom)
        };
        let canvas_to_screen = |rect: Rectangle<f64, Logical>| {
            Rectangle::new(
                Point::from((
                    (rect.loc.x - camera.x) * zoom,
                    (rect.loc.y - camera.y) * zoom,
                )),
                Size::from((rect.size.w * zoom, rect.size.h * zoom)),
            )
        };
        let id = self.stage.id_of(window);
        if let Some(id) = id
            && let Some(visual) = self.window_animations.geometry_visual_rect(id)
        {
            match self.window_animations.geometry_space(id)? {
                // A pinned entry already chases in screen px at zoom 1 — but
                // only on the output it is pinned to. Another output's entry
                // says nothing about this one, so fall through to the live
                // answer rather than hand back a neighbour's coordinates.
                AnimSpace::Screen(name) if name == output.name() => return Some(visual),
                AnimSpace::Screen(_) => {}
                AnimSpace::Canvas => return Some(canvas_to_screen(visual)),
            }
        }
        let size = window.geometry().size.to_f64();
        match self.stage.pin_of(window) {
            Some(site) if site.output == output.name() => {
                Some(Rectangle::new(site.screen_pos.to_f64(), size))
            }
            Some(_) => None,
            None => {
                let loc = self.stage.position_of(window)?;
                Some(canvas_to_screen(Rectangle::new(loc.to_f64(), size)))
            }
        }
    }

    /// Seed rect for a fresh geometry entry: an in-flight chase's own visual,
    /// so an interruption stays continuous, else the rect at `loc`/`size`.
    ///
    /// Deliberately not the *drawn* rect: an open fade's shrink is carried onto
    /// the new chase and re-applied at draw time, so baking it into the seed
    /// would scale the arrival twice.
    pub(crate) fn geometry_seed(
        &self,
        id: ElementId,
        loc: Point<i32, Logical>,
        size: Size<i32, Logical>,
    ) -> Rectangle<f64, Logical> {
        self.window_animations
            .geometry_visual_rect(id)
            .unwrap_or_else(|| Rectangle::new(loc.to_f64(), size.to_f64()))
    }

    /// Where a stand-in's departing picture is right now: mid-slide that is the
    /// leg's own visual, not the destination the stage already holds. A dismiss
    /// freezes one rect for the fade's whole life and judges drawability on it,
    /// so reading the destination would both teleport the chrome there for frame
    /// zero and skip the fade outright while the slide is still on screen.
    pub(crate) fn departing_standin_rect(
        &self,
        element: &StageWindow,
    ) -> Option<Rectangle<f64, Logical>> {
        let id = self.stage.id_of(element)?;
        let loc = self.stage.position_of(element)?;
        Some(self.geometry_seed(id, loc, StageElement::size(element)))
    }

    /// Canvas geometry animation toward a size configure (fill/fit). Must be
    /// called while the stage still holds the pre-action rect; the chase target
    /// is then the new live stage position. `final_loc` is that post-action
    /// position, for a caller that can run inside a window's map commit (see
    /// [`Self::start_geometry_entry`]) — the stage does not hold it yet.
    pub(crate) fn animate_window_geometry(
        &mut self,
        window: &Window,
        to_size: Size<i32, Logical>,
        final_loc: Option<Point<i32, Logical>>,
    ) {
        let Some(id) = self.stage.id_of(window) else {
            return;
        };
        let old_loc = self.stage.position_of(window).unwrap_or_default();
        let seed = self.geometry_seed(id, old_loc, window.geometry().size);
        self.start_geometry_entry(
            &StageWindow::Client(window.clone()),
            seed,
            AnimSpace::Canvas,
            Some(to_size),
            GeometryRole::Normal,
            false,
            ContentPolicy::Cap,
            None,
            final_loc,
        );
    }

    /// Geometry animation with an explicit, frame-converted seed (fullscreen
    /// enter/exit cross the locked-viewport ↔ camera ↔ pin-screen boundary).
    /// `final_loc` as in [`Self::start_geometry_entry`].
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn begin_geometry_animation_seeded(
        &mut self,
        window: &Window,
        seed: Rectangle<f64, Logical>,
        space: AnimSpace,
        requested_size: Option<Size<i32, Logical>>,
        role: GeometryRole,
        content_policy: ContentPolicy,
        final_loc: Option<Point<i32, Logical>>,
    ) {
        self.start_geometry_entry(
            &StageWindow::Client(window.clone()),
            seed,
            space,
            requested_size,
            role,
            true,
            content_policy,
            None,
            final_loc,
        );
    }

    /// [`Self::animate_element_move_from`] for a client window.
    #[cfg(test)]
    pub(crate) fn animate_window_move_from(
        &mut self,
        window: &Window,
        from_loc: Point<i32, Logical>,
        waits_for: Option<ElementId>,
    ) {
        self.animate_element_move_from(&StageWindow::Client(window.clone()), from_loc, waits_for);
    }

    /// Position-only canvas animation from `from_loc` (nudge, cluster shift),
    /// for any stage element — a suspended stand-in slides like the window it
    /// stands for. The stage already holds the new position; the seed pins the
    /// old one.
    ///
    /// `waits_for` names the entry this one is being pushed by, if any: the leg
    /// stays parked on the seed until that entry's own resize freeze releases,
    /// so a pushed neighbour and the window pushing it move as one.
    pub(crate) fn animate_element_move_from(
        &mut self,
        element: &StageWindow,
        from_loc: Point<i32, Logical>,
        waits_for: Option<ElementId>,
    ) {
        let Some(id) = self.stage.id_of(element) else {
            return;
        };
        let size = element.geometry().size;
        // Keep an in-flight entry's visual; otherwise seed at the old position.
        let seed = self.geometry_seed(id, from_loc, size);
        self.start_geometry_entry(
            element,
            seed,
            AnimSpace::Canvas,
            None,
            GeometryRole::Normal,
            false,
            ContentPolicy::Cap,
            waits_for,
            None,
        );
    }

    /// Whether one window's picture currently covers `output` edge to edge as a
    /// fullscreen one. True in neither direction *while the transition plays*:
    /// an entry keeps the previous scene visible until the window reaches the
    /// output bounds, and an exit's freeze holds the fullscreen picture on screen
    /// after the stage has already let it go — a waybar popping in over a
    /// motionless fullscreen frame is the same leak from the other side.
    pub(crate) fn is_output_visually_fullscreen(&self, output: &Output) -> bool {
        if self.frozen_fullscreen_cover(output).is_some() {
            return true;
        }
        self.is_output_fullscreen(output) && self.fullscreen_entry_on(output).is_none()
    }

    /// The frozen fullscreen picture still covering `output` — one held under the
    /// view the output is showing right now.
    ///
    /// Judged against [`Self::world_view`], not the live viewport: the picture is
    /// drawn through that view, and a fullscreen entry parks the live one a whole
    /// transition ahead of what is on screen. Reading live would drop the claim
    /// of a picture that has not moved (a handover from a zoomed-out canvas, where
    /// the park is the only difference between the two) and keep the claim of one
    /// that has (a zoom during the freeze, later swallowed by a park that happens
    /// to round back). The two agree whenever nothing is entering fullscreen.
    pub(crate) fn frozen_fullscreen_cover(&self, output: &Output) -> Option<ElementId> {
        let (camera, zoom) = self.world_view(output);
        self.window_animations
            .frozen_fullscreen_on(&output.name(), camera, zoom)
    }

    /// The camera and zoom the *scene* on `output` renders through: its live
    /// viewport, except while a fullscreen entry is growing, when everything but
    /// the entering window keeps the pre-fullscreen view (see `compose_frame`).
    /// The background caches gate their redraws on this, so they have to agree
    /// with the composer about which view a frame was drawn at.
    ///
    /// Only what is *drawn* moves to this view. The pointer's canvas position is
    /// warped into the parked frame when fullscreen is entered, so the cursor and
    /// every hit test stay on the live one — for the length of the entry a click
    /// on the scene behind lands where the parked view puts it, not under what is
    /// drawn. Accepted: the entry is a few frames, and it ends with that scene
    /// culled entirely.
    pub(crate) fn world_view(&self, output: &Output) -> (Point<f64, Logical>, f64) {
        let (camera, zoom, parked) = {
            let os = output_state(output);
            (
                os.camera,
                os.zoom,
                os.fullscreen_return.as_ref().map(|r| (r.camera, r.zoom)),
            )
        };
        match parked {
            Some(view) if self.fullscreen_entry_on(output).is_some() => view,
            _ => (camera, zoom),
        }
    }

    /// The window whose fullscreen *entry* is still growing on `output`. Until it
    /// lands, the output is not covered and the scene behind it still shows.
    pub(crate) fn fullscreen_entry_on(&self, output: &Output) -> Option<Window> {
        let window = self.fullscreen_window_on(output)?;
        let id = self.stage.id_of(&window)?;
        self.window_animations
            .fullscreen_entry_active(id)
            .then_some(window)
    }

    /// The canvas rect the frame composer culls a window on: the bounding box it
    /// occupies live, merged with the rect an animation is currently drawing it
    /// at. The two can be far apart — a resize that also moves puts the live rect
    /// off the viewport while the frozen picture is still on it — and redraws are
    /// already scoped by the animated rect, so culling on the live one alone
    /// composes the window out of the very frames its animation asked for.
    pub(crate) fn window_cull_rect(
        &self,
        id: Option<ElementId>,
        bbox: Rectangle<i32, Logical>,
    ) -> Rectangle<i32, Logical> {
        match id.and_then(|id| self.window_animations.canvas_visual_rect(id)) {
            Some(visual) => bbox.merge(visual.to_i32_round()),
            None => bbox,
        }
    }

    /// The windows a visually fullscreen `output` shows, and the only ones its
    /// cull keeps. Normally just the stage's fullscreen window; through an exit
    /// freeze it is the window on its way out, which the stage no longer lists
    /// there — without it the cull that hides everything under a fullscreen
    /// picture would hide that picture too.
    ///
    /// Both, when one window's fullscreen is being handed to another: the
    /// outgoing exit's freeze still holds its picture across the output while
    /// the incoming window grows into it. Keeping one would compose the incoming
    /// window out of every frame of its own growth. Everything else on the output
    /// really is hidden — the stage keeps fullscreen windows topmost, so nothing
    /// but these two is above the picture doing the covering.
    pub(crate) fn visually_fullscreen_windows_on(&self, output: &Output) -> Vec<Window> {
        let mut windows: Vec<Window> = Vec::new();
        let covering = self
            .frozen_fullscreen_cover(output)
            .and_then(|id| self.stage.window_by_id(id))
            .and_then(|element| element.client())
            .cloned();
        windows.extend(covering);
        // The same window on both sides is one picture, not two: a re-entered
        // fullscreen exits and enters on one element.
        if let Some(live) = self.fullscreen_window_on(output)
            && !windows.contains(&live)
        {
            windows.push(live);
        }
        windows
    }

    pub(crate) fn tick_window_animations(&mut self, dt: Duration) {
        self.tick_window_animations_at(dt, Instant::now());
    }

    /// Advance every window animation, closing snapshot, and adoption fade.
    /// `now` is injectable so tests drive endpoint-hold deadlines deterministically.
    pub(crate) fn tick_window_animations_at(&mut self, dt: Duration, now: Instant) {
        let speed = self.config.effects.animation_speed;
        let frame_factor = 1.0 - (1.0 - speed).powf(dt.as_secs_f64() * 60.0);

        // Mark the outputs that show a *moving* animation this tick, before
        // advancing, so the completing tick still presents the final resting
        // frame and udev re-arms the next frame (rect-scoped; never
        // mark_all_dirty). A frozen entry holds one picture still, so it isn't a
        // reason to compose — but it still counts toward `redraws_needed` on
        // udev, which is what pumps the ticks its own deadline needs to fire.
        let affected: Vec<Output> = self
            .space
            .outputs()
            .filter(|o| {
                let (camera, zoom) = {
                    let os = output_state(o);
                    (os.camera, os.zoom)
                };
                self.output_shows_window_animations(o, camera, zoom, Some(now))
            })
            .cloned()
            .collect();

        for (id, geo) in self.window_animations.scoping_entries() {
            // An entry whose element or pin has vanished mid-chase can never be
            // ticked to convergence; drop it (same instant-complete outcome as
            // ineligible) so it can't wedge `has_active_animations` true forever.
            let Some(element) = self.stage.window_by_id(id).cloned() else {
                self.window_animations.remove(id);
                self.drop_resize_crossfade(id);
                continue;
            };
            let live_size = StageElement::size(&element).to_f64();
            let target = match &geo {
                Some((AnimSpace::Screen(name), _)) => self
                    .stage
                    .pin_of(&element)
                    .map(|site| (site.screen_pos.to_f64(), self.output_name_drawable(name))),
                Some((AnimSpace::Canvas, visual)) => self.stage.position_of(&element).map(|loc| {
                    (
                        loc.to_f64(),
                        self.canvas_rect_drawable(visual.to_i32_round()),
                    )
                }),
                None => self.stage.position_of(&element).map(|loc| {
                    let rect = Rectangle::new(loc, StageElement::size(&element));
                    (loc.to_f64(), self.canvas_rect_drawable(rect))
                }),
            };
            let Some((target_loc, eligible)) = target else {
                self.window_animations.remove(id);
                self.drop_resize_crossfade(id);
                continue;
            };
            let keep = self.window_animations.tick_entry(
                id,
                target_loc,
                live_size,
                frame_factor,
                now,
                eligible,
            );
            // The leg is moving now, so a view move parked on it goes with it —
            // the budget expiring fires it too, since that leg starts moving as
            // well. Taken before the removal below, which would otherwise drop it
            // silently: an entry that instant-completes (it covers no drawable
            // output) still owes the move, it just has nothing left to wait for.
            let released_view = if !keep || !self.window_animations.start_held(id) {
                self.window_animations.take_pending_view(id)
            } else {
                None
            };
            if !keep {
                self.window_animations.remove(id);
            }
            if !eligible {
                // Instant-completed off-screen: there is no leg left to fade over.
                self.drop_resize_crossfade(id);
            } else if !self.window_animations.start_held(id) {
                // Captured content is only good while the window is frozen. Any
                // other exit from the freeze (the budget expiring) leaves stale
                // pixels for a leg that already runs with them stretched.
                self.resize_captures.drop_for(id);
            }
            if let Some(pending) = released_view {
                self.land_or_defer_view(&element, pending);
            }
        }

        for snapshot in &mut self.closing_snapshots {
            snapshot.tick(frame_factor);
        }
        self.closing_snapshots.retain(|s| !s.is_done());

        for crossfade in self.resize_crossfades.values_mut() {
            crossfade.tick(frame_factor);
        }
        self.resize_crossfades.retain(|_, c| !c.is_done());

        let mut faded: Vec<crate::state::SuspendedId> = Vec::new();
        for fade in &mut self.standin_fades {
            fade.tick(frame_factor);
        }
        self.standin_fades.retain(|fade| {
            if fade.is_done() {
                faded.push(fade.suspended.id);
                false
            } else {
                true
            }
        });
        // The fade re-inserted suspended chrome its owner purged; re-purge it.
        for sid in faded {
            let key = crate::decorations::DecorationKey::Suspended(sid);
            self.decorations.remove(&key);
            self.render.border_cache.remove(&key);
            self.render.shadow_cache.remove(&key);
        }

        for output in affected {
            self.redraws_needed.insert(output);
        }
    }

    /// Land the view move `element`'s entry just released, or hold it until the
    /// grab that would be warped by it lets go.
    ///
    /// A grab install clears the camera targets [`Self::apply_pending_view`]
    /// treats as "a later action owns the view", so without this the pan would
    /// land *more* readily under a grab than without one — straight into
    /// something measuring its delta against a frozen canvas anchor.
    fn land_or_defer_view(&mut self, element: &StageWindow, pending: PendingView) {
        if self.view_warps_a_live_grab(element, &pending) {
            self.deferred_views.insert(pending.output.clone(), pending);
            return;
        }
        self.apply_pending_view(pending);
    }

    /// Whether landing `pending` would feed synthetic motion to a grab that did
    /// not ask for it.
    fn view_warps_a_live_grab(&self, element: &StageWindow, pending: &PendingView) -> bool {
        // `warp_pointer` reaches a grab on exactly this condition, and only on
        // it: `interactive_move` would miss every client resize, which installs
        // a grab measuring against the same frozen anchor without registering as
        // an interactive move.
        if !self
            .seat
            .get_pointer()
            .is_some_and(|pointer| pointer.is_grabbed())
        {
            return false;
        }
        // The dragged element is exempt: `end_element_animation` hands this same
        // pan to a drag that interrupts the fit, so the user's own action
        // inherits the promise on whichever release path gets there first.
        if self.element_under_interactive_move(element) {
            return false;
        }
        // Only the active output's camera warps the pointer (every tick that
        // calls `warp_pointer` is `is_active`-gated), so a flight staged for any
        // other output cannot reach the grab.
        self.active_output()
            .is_some_and(|output| output.name() == pending.output)
    }

    /// Hand over the view moves a grab held back. Called from the grab teardowns
    /// rather than polled per frame: landing a pan is itself what makes the
    /// compositor non-idle, so a check that only ran on an already-live frame
    /// would never fire on the release that ends all activity.
    ///
    /// Reads no `PointerHandle` — a grab's `unset` runs inside the pointer mutex.
    /// The `interactive_move` check stands in for it: the pointer grab is on its
    /// way out by definition, and any *other* grab still holding one is on that
    /// list and will flush on its own release.
    pub(crate) fn flush_deferred_views(&mut self) {
        if self.deferred_views.is_empty() || !self.interactive_move.is_empty() {
            return;
        }
        for pending in std::mem::take(&mut self.deferred_views).into_values() {
            self.apply_pending_view(pending);
        }
    }

    /// Land a view move that was waiting on a window's freeze, into the output
    /// it was staged for — never through `with_output_state`, which resolves
    /// the live active output instead.
    fn apply_pending_view(&mut self, pending: PendingView) {
        let Some(output) = self.output_by_name(&pending.output) else {
            return;
        };
        // A fullscreen output's camera is locked and the per-tick clear wipes
        // these targets anyway, so writing them is pure churn.
        if self.is_output_fullscreen(&output) {
            return;
        }
        let mut os = output_state(&output);
        // Compared exactly, like the fullscreen cover's stamp — a camera that
        // drifted from `staged_camera` by even a hair took ownership of the view.
        if os.camera != pending.staged_camera || os.zoom != pending.staged_zoom {
            return;
        }
        // Staging cleared both targets, so a target that exists now was armed by
        // a later action — one whose own move has not started travelling yet, and
        // so leaves no trace in the camera itself for the check above to catch.
        if os.camera_target.is_some() || os.zoom_target.is_some() {
            return;
        }
        os.momentum.stop();
        os.zoom_animation_anchor = Some(pending.anchor);
        os.camera_target = Some(pending.camera);
        os.zoom_target = Some(pending.zoom);
    }

    /// Resolve the outstanding request on a commit of an animated window. A
    /// commit that releases a start hold is also the crossfade's cue — the one
    /// moment the old and new pictures both exist.
    pub(crate) fn resolve_window_animation_commit(&mut self, window: &Window) {
        let Some(id) = self.stage.id_of(window) else {
            return;
        };
        let committed_size = window.geometry().size;
        let released = self.window_animations.on_window_commit(id, committed_size);
        if let Some(generation) = released {
            self.start_resize_crossfade(window, id, generation, committed_size);
        }
    }

    /// Clone the textures a frozen window is about to replace, so its resize leg
    /// has an old picture to fade out. Cheap (Rc clones, no GPU work) and bounded
    /// by the freeze — a commit or two; each refresh replaces the last, so the
    /// fade starts from what was actually on screen. Renderer-gated, like every
    /// capture path (the flatten needs one anyway).
    pub(crate) fn stash_resize_content(&mut self, surface: &WlSurface) {
        // This hook runs on every commit of every surface, so cheap-out on the
        // O(1) checks before the surface-state read and the O(#windows) stage
        // lookup. Only a frozen entry stashes anything, and most animations
        // (open, moves) never freeze at all.
        if !self.window_animations.any_start_held() {
            return;
        }
        let new_buffer = with_states(surface, |states| {
            matches!(
                states
                    .cached_state
                    .get::<SurfaceAttributes>()
                    .pending()
                    .buffer,
                Some(BufferAssignment::NewBuffer(_))
            )
        });
        if !new_buffer {
            return;
        }
        let Some(window) = self.window_for_surface(surface) else {
            return;
        };
        let Some(id) = self.stage.id_of(&window) else {
            return;
        };
        if !self.window_animations.start_held(id) {
            return;
        }
        // A window fading in has shown no old picture to cross from, so its
        // client's throwaway first buffer must not be stretched under the fade.
        if self.window_animations.has_open_fade(id) {
            return;
        }
        let Some(generation) = self.window_animations.generation_of(id) else {
            return;
        };
        // Pre-commit, both the textures and the geometry still describe the
        // picture being retired — and so does the chrome around it. Resolve that
        // here too: by the time this is baked, a config reload or the fullscreen
        // membership this freeze is riding could answer differently.
        let geometry = window.geometry();
        let chrome = self.baked_chrome_policy(surface, self.chrome_fullscreen(&window));
        let Some(mut backend) = self.backend.take() else {
            return;
        };
        if let Some(pixels) = crate::render::capture_close_pixels(
            backend.renderer(),
            surface,
            geometry,
            Instant::now(),
        ) {
            self.resize_captures.stash(id, pixels, chrome, generation);
        }
        self.backend = Some(backend);
    }

    /// Flatten the content stashed while `window` was frozen into the fading half
    /// of its resize crossfade. The stash is consumed either way; a generation
    /// mismatch means it belongs to a superseded request, so it is dropped
    /// rather than paired with this leg. Backend-gated.
    ///
    /// `committed_size` is the size the resolving commit landed, threaded down
    /// rather than re-read: the crossfade's direction has to be decided from the
    /// same size the leg resolved on, and a second `geometry()` read only looks
    /// like it guarantees that.
    fn start_resize_crossfade(
        &mut self,
        window: &Window,
        id: ElementId,
        generation: u64,
        committed_size: Size<i32, Logical>,
    ) {
        let Some(capture) = self.resize_captures.take_for(id, generation) else {
            return;
        };
        // At full speed the overlay is done on the very next tick, before it can
        // ever be composed — so the GPU flatten it needs is pure waste.
        if self.config.effects.animation_speed >= 1.0 {
            return;
        }
        let corner_clip = self.render.corner_clip_shader.clone();
        let flatten_scale = self.resize_bake_scale(window, id, capture.pixels.geometry.size);
        let Some(mut backend) = self.backend.take() else {
            return;
        };
        let crossfade = crate::render::resize_crossfade(
            backend.renderer(),
            &capture.pixels,
            committed_size,
            flatten_scale,
            corner_clip.as_ref(),
            capture.chrome,
        );
        self.backend = Some(backend);
        match crossfade {
            Some(crossfade) => {
                self.resize_crossfades.insert(id, crossfade);
            }
            // The resize still animates, just without the old content fading over
            // it — a degrade the user might notice and the log otherwise wouldn't.
            None => tracing::warn!("resize crossfade bake failed; animating without it"),
        }
    }

    /// Texels per captured logical px a resize bake needs so one baked texel
    /// lands on one physical pixel. The overlay's first frame paints the bake
    /// over the frozen visual rect, which can be several times the rect the
    /// content was captured at (a fullscreen exit restores into a zoomed-out
    /// camera, where the rect is `screen / zoom`), and that rect is then drawn
    /// through the same transform as live content — so the two factors multiply.
    /// Deliberately unfloored: `resize_crossfade` applies the only floor this
    /// needs, and flooring the render-scale half here would over-rasterize a
    /// zoomed-out bake by `1 / (output_scale · zoom)`.
    pub(crate) fn resize_bake_scale(
        &self,
        window: &Window,
        id: ElementId,
        captured: Size<i32, Logical>,
    ) -> f64 {
        let render_scale = match self.stage.pin_of(window) {
            // A pinned window draws at zoom 1 on its own output.
            Some(site) => self
                .output_by_name(&site.output)
                .map_or(1.0, |o| o.current_scale().fractional_scale()),
            None => {
                let stage_pos = self.stage.position_of(window).unwrap_or_default();
                self.canvas_rect_render_scale(Rectangle::new(stage_pos, captured))
                    .unwrap_or(1.0)
            }
        };
        let stretch = self
            .window_animations
            .geometry_visual_rect(id)
            .map_or(1.0, |visual| {
                (visual.size.w / captured.w.max(1) as f64)
                    .max(visual.size.h / captured.h.max(1) as f64)
            });
        render_scale * stretch
    }

    /// The chrome policy a bake has to reproduce: whether the window draws bare
    /// (a fullscreen window has no compositor chrome live, and
    /// `decoration = "none"` hard-vetoes it) and the per-corner radius the live
    /// clip applies. Under an SSD bar only the bottom corners round — the bar
    /// covers the top edge.
    fn baked_chrome_policy(
        &self,
        surface: &WlSurface,
        fullscreen: bool,
    ) -> crate::render::BakeChrome {
        let applied = driftwm::config::applied_rule(surface);
        let mode = driftwm::config::effective_decoration_mode(
            applied.as_ref().and_then(|r| r.decoration.as_ref()),
            &self.config.decorations.default_mode,
        );
        if fullscreen || matches!(mode, driftwm::config::DecorationMode::None) {
            return crate::render::BakeChrome {
                bare: true,
                corner_radius: [0.0; 4],
            };
        }
        let radius = driftwm::config::effective_corner_radius(
            applied.as_ref(),
            mode,
            &self.config.decorations,
        ) as f32;
        let has_bar = self
            .decorations
            .contains_key(&crate::decorations::DecorationKey::Surface(surface.id()));
        let corner_radius = if has_bar {
            [0.0, 0.0, radius, radius]
        } else {
            [radius; 4]
        };
        crate::render::BakeChrome {
            bare: false,
            corner_radius,
        }
    }

    /// Drop both halves of a resize crossfade for `id`: content stashed for a
    /// flatten that will not happen, and an overlay already fading. Called
    /// wherever the geometry entry itself is dropped — the id survives
    /// `Stage::replace`, so the dead-id sweep alone would leave a stale overlay
    /// on a stand-in.
    pub(crate) fn drop_resize_crossfade(&mut self, id: ElementId) {
        self.resize_captures.drop_for(id);
        self.resize_crossfades.remove(&id);
    }

    /// Physical px per logical px a canvas rect is drawn at: the max
    /// `output_scale·zoom` among the outputs whose viewport intersects it, or
    /// `None` when none does.
    fn canvas_rect_render_scale(&self, rect: Rectangle<i32, Logical>) -> Option<f64> {
        self.space
            .outputs()
            .filter_map(|o| {
                let (camera, zoom) = {
                    let os = output_state(o);
                    (os.camera, os.zoom)
                };
                let viewport = super::output_logical_size(o);
                let visible =
                    driftwm::canvas::visible_canvas_rect(camera.to_i32_round(), viewport, zoom);
                visible
                    .overlaps(rect)
                    .then(|| o.current_scale().fractional_scale() * zoom)
            })
            .fold(None, |best: Option<f64>, s| {
                Some(best.map_or(s, |b| b.max(s)))
            })
    }

    /// Rasterization scale for a closing window's bake: the rect's render scale,
    /// never below 1.0, so a zoomed-out close still bakes at full logical
    /// resolution (as does a rect no output currently shows).
    pub(crate) fn flatten_scale_for_canvas_rect(&self, rect: Rectangle<i32, Logical>) -> f64 {
        self.canvas_rect_render_scale(rect)
            .map_or(1.0, |scale| scale.max(1.0))
    }

    /// Flatten the captured content of a closing window into a queued snapshot
    /// (backend-gated, consumes the captured close pixels). `fullscreen_output`
    /// picks screen-space placement on that output (or the pin's output when
    /// pinned) vs. canvas space otherwise. `alpha_only` fades in place at scale
    /// 1, for the suspend-conversion crossfade.
    pub(crate) fn snapshot_closing_window(
        &mut self,
        window: &Window,
        surface: &WlSurface,
        fullscreen_output: Option<&Output>,
        alpha_only: bool,
    ) {
        // A window awaiting a deferred adopt has never been drawn where it sits,
        // and the capture below imports its buffers on demand rather than
        // reusing what a frame drew — so without this the fade-out would be the
        // first and only time the user sees it.
        if self.root_hidden_by_deferred_adopt(surface) {
            return;
        }
        // Backend-gated (the headless fixture never accumulates render transients).
        let Some(mut backend) = self.backend.take() else {
            return;
        };
        let id = surface.id();
        if !self.close_pixels.contains_key(&id)
            && let Some(px) = crate::render::capture_close_pixels(
                backend.renderer(),
                surface,
                window.geometry(),
                Instant::now(),
            )
        {
            self.close_pixels.insert(id.clone(), px);
        }
        let Some(px) = self.close_pixels.remove(&id) else {
            self.backend = Some(backend);
            return;
        };
        let scale_amplitude = self.config.effects.animation_scale;
        // The rect recorded with the pixels, never live geometry: a client that
        // unmapped before destroying its toplevel already reports a zero-sized
        // window, which would collapse the bake and silently drop the animation.
        let geom_loc = px.geometry.loc;
        let geom_size = px.geometry.size;

        // Off-screen closes never show — skip the flatten entirely. Stale pixels
        // don't either: the unmap hook fires on every hide, so a hide-to-tray app
        // that quits much later must not fade in what it looked like back then.
        let fresh = crate::render::close_pixels_fresh(px.captured_at, Instant::now());
        let drawable = fresh
            && if let Some(output) = fullscreen_output {
                self.output_name_drawable(&output.name())
            } else if let Some(site) = self.stage.pin_of(window) {
                self.output_name_drawable(&site.output)
            } else {
                let stage_pos = self.stage.position_of(window).unwrap_or_default();
                self.canvas_rect_drawable(Rectangle::new(stage_pos, geom_size))
            };
        if !drawable {
            self.backend = Some(backend);
            return;
        }
        // Resolve the live chrome so the fade starts from the picture the window
        // actually had. Everything is still intact here (pre-`cleanup_surface_state`),
        // and the rects are surface-origin-local for the bake. A fullscreen window
        // has no chrome live, so it bakes bare.
        let corner_clip = self.render.corner_clip_shader.clone();
        let border_shader = self.render.border_shader.clone();
        let shadow_shader = self.render.shadow_shader.clone();
        // A bare window bakes with no clip, no border and no shadow, matching the
        // nothing it draws live.
        let policy = self.baked_chrome_policy(surface, fullscreen_output.is_some());
        let corner_radius = policy.corner_radius;
        let chrome = if policy.bare {
            None
        } else {
            let applied = driftwm::config::applied_rule(surface);
            let mode = driftwm::config::effective_decoration_mode(
                applied.as_ref().and_then(|r| r.decoration.as_ref()),
                &self.config.decorations.default_mode,
            );
            let bw = driftwm::config::effective_border_width(
                applied.as_ref(),
                mode,
                &self.config.decorations,
            );
            let focused = self
                .seat
                .get_keyboard()
                .and_then(|kb| kb.current_focus())
                .is_some_and(|f| f.0 == *surface);
            let border_color = if focused {
                driftwm::config::effective_border_color_focused(
                    applied.as_ref(),
                    &self.config.decorations,
                )
            } else {
                driftwm::config::effective_border_color(applied.as_ref(), &self.config.decorations)
            };
            let shadow_on = driftwm::config::effective_shadow_enabled(
                applied.as_ref(),
                mode,
                &self.config.decorations,
            );
            let bar_h = self.config.decorations.title_bar_height;
            let deco_key = crate::decorations::DecorationKey::Surface(id.clone());
            let bar = self.decorations.get(&deco_key).map(|d| {
                (
                    &d.title_bar,
                    Rectangle::new(
                        Point::from((geom_loc.x as f64, (geom_loc.y - bar_h) as f64)),
                        Size::from((geom_size.w as f64, bar_h as f64)),
                    ),
                )
            });
            Some(crate::render::CloseChrome {
                geometry: Rectangle::new(geom_loc.to_f64(), geom_size.to_f64()),
                corner_radius,
                corner_clip: corner_clip.as_ref(),
                border_shader: border_shader.as_ref(),
                border_width: bw,
                border_color,
                focused,
                shadow_shader: shadow_on.then_some(shadow_shader.as_ref()).flatten(),
                bar,
            })
        };
        let chrome = chrome.as_ref();
        let snapshot = if let Some(output) = fullscreen_output {
            let flatten_scale = output.current_scale().fractional_scale();
            crate::render::snapshot_screen(
                backend.renderer(),
                &px,
                output.name(),
                Point::from((-geom_loc.x, -geom_loc.y)),
                flatten_scale,
                scale_amplitude,
                alpha_only,
                chrome,
            )
        } else if let Some(site) = self.stage.pin_of(window).cloned() {
            let flatten_scale = self
                .output_by_name(&site.output)
                .map(|o| o.current_scale().fractional_scale())
                .unwrap_or(1.0);
            let screen_origin = Point::from((
                site.screen_pos.x - geom_loc.x,
                site.screen_pos.y - geom_loc.y,
            ));
            crate::render::snapshot_screen(
                backend.renderer(),
                &px,
                site.output,
                screen_origin,
                flatten_scale,
                scale_amplitude,
                alpha_only,
                chrome,
            )
        } else {
            let stage_pos = self.stage.position_of(window).unwrap_or_default();
            let window_origin = Point::from((
                (stage_pos.x - geom_loc.x) as f64,
                (stage_pos.y - geom_loc.y) as f64,
            ));
            let flatten_scale =
                self.flatten_scale_for_canvas_rect(Rectangle::new(stage_pos, geom_size));
            crate::render::snapshot_canvas(
                backend.renderer(),
                &px,
                window_origin,
                flatten_scale,
                scale_amplitude,
                alpha_only,
                chrome,
            )
        };
        self.backend = Some(backend);
        if let Some(snapshot) = snapshot {
            self.closing_snapshots.push(snapshot);
        }
    }

    /// Whether any window animation, closing snapshot, or adoption fade has a
    /// visual rect intersecting `output`'s viewport. Caller passes the output's
    /// already-read camera/zoom so this never re-locks `output_state`.
    ///
    /// `frozen_cutoff` is `Some(now)` for the redraw side: an entry still frozen
    /// at `now` repaints the identical picture every frame, so it is not a reason
    /// to compose one.
    pub(super) fn output_shows_window_animations(
        &self,
        output: &Output,
        camera: Point<f64, Logical>,
        zoom: f64,
        frozen_cutoff: Option<Instant>,
    ) -> bool {
        let name = output.name();
        let viewport = output_logical_size(output);
        let visible = driftwm::canvas::visible_canvas_rect(camera.to_i32_round(), viewport, zoom);

        for snapshot in &self.closing_snapshots {
            match snapshot.pinned_output() {
                Some(o) => {
                    if o == name {
                        return true;
                    }
                }
                None => {
                    if visible.overlaps(snapshot.canvas_rect().to_i32_round()) {
                        return true;
                    }
                }
            }
        }
        for fade in &self.standin_fades {
            let rect = Rectangle::new(fade.loc, fade.suspended.size.get());
            if visible.overlaps(rect) {
                return true;
            }
        }
        // A crossfade outlives its leg only by a tick or two, but it rides the
        // window's live rect for scoping either way.
        for id in self.resize_crossfades.keys() {
            if let Some(rect) = self.animation_open_canvas_rect(*id)
                && visible.overlaps(rect)
            {
                return true;
            }
        }
        for (id, geo) in self.window_animations.scoping_entries() {
            if frozen_cutoff.is_some_and(|now| self.window_animations.frozen_at(id, now)) {
                continue;
            }
            match geo {
                Some((super::window_animation::AnimSpace::Screen(o), _)) => {
                    if o == name {
                        return true;
                    }
                }
                Some((super::window_animation::AnimSpace::Canvas, rect)) => {
                    if visible.overlaps(rect.to_i32_round()) {
                        return true;
                    }
                }
                None => {
                    if let Some(rect) = self.animation_open_canvas_rect(id)
                        && visible.overlaps(rect)
                    {
                        return true;
                    }
                }
            }
        }
        false
    }

    /// Live canvas rect of a window whose effect has no rect of its own — an
    /// open entry, a resize crossfade (used for scoping only).
    fn animation_open_canvas_rect(
        &self,
        id: driftwm::stage::ElementId,
    ) -> Option<Rectangle<i32, Logical>> {
        let window = self.stage.window_by_id(id)?;
        let loc = self.stage.position_of(window)?;
        Some(Rectangle::new(
            loc,
            driftwm::stage::StageElement::size(window),
        ))
    }
}
