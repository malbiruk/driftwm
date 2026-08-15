use std::collections::HashMap;
use std::time::{Duration, Instant};

use smithay::utils::{Logical, Point, Rectangle, Size};

use driftwm::stage::ElementId;

use super::PendingView;

/// Every effect advances on normalized progress and ends when within this of
/// 1.0. Deliberately larger than the camera's convergence epsilon: a tighter one
/// leaves a long, invisible tail well past the visible motion. Because geometry
/// rides the same scalar, its settle time is a fixed duration rather than growing
/// with travel distance the way a distance-epsilon chase does.
const PROGRESS_DONE_EPSILON: f64 = 0.01;
/// A geometry target that moves by more than this many logical pixels (per axis,
/// location or size) re-seeds the lerp instead of stretching the current leg.
const TARGET_MOVED_EPSILON: f64 = 0.5;
/// A client that never commits the requested size holds the stretched endpoint
/// no longer than this after first reaching it.
pub(crate) const MAX_ENDPOINT_HOLD: Duration = Duration::from_millis(500);
/// How long a compositor-initiated resize waits for the client's first redraw
/// before giving up and animating with stale content. Deliberately shorter than
/// the endpoint hold, which the eye reads as a settled window lingering; this one
/// is a keystroke answered by nothing moving at all.
pub(crate) const MAX_START_HOLD: Duration = Duration::from_millis(300);
/// A compositor resize smaller than this (per axis) carries no request at all.
/// It is not worth freezing the window, stashing its content, flattening that on
/// the GPU and crossfading it — and worse, a client that *cannot* honour a small
/// request (cell-quantized terminals, aspect-locked players, fixed-size dialogs)
/// answers by committing its old size, which is indistinguishable from not
/// answering, so the freeze would burn its whole budget over a resize nobody can
/// see. The geometry leg still runs; it just has nothing to wait for.
pub(crate) const MIN_ANIMATED_RESIZE: i32 = 10;

/// The freeze that precedes a compositor-initiated resize leg. Nothing moves
/// until the client delivers the new size, so the leg can play with real content
/// on both sides of the crossfade instead of stretching a stale buffer.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum StartHold {
    /// Not holding — the leg advances normally.
    Off,
    /// Armed at seed; the deadline anchors on the first tick, like the endpoint
    /// hold, so a queued entry doesn't burn its budget before it is ticked.
    Armed,
    /// Frozen until this deadline, after which the leg degrades to animating
    /// with capped stale content.
    Until(Instant),
}

impl StartHold {
    pub fn is_held(self) -> bool {
        !matches!(self, StartHold::Off)
    }

    /// Whether this hold still holds after a tick at `now`. The tick that lets
    /// a budget expire answers false — that one does move the window.
    fn holds_at(self, now: Instant) -> bool {
        match self {
            StartHold::Off => false,
            StartHold::Armed => true,
            StartHold::Until(deadline) => now < deadline,
        }
    }
}

/// The rendered stand-in for a window while an animation is playing: where its
/// content is drawn, at what size, and how opaque. Named apart from the
/// `window_visual_*` family (which reports the pin-aware logical rect).
#[derive(Clone, Copy, Debug)]
pub(crate) struct AnimatedVisual {
    pub loc: Point<f64, Logical>,
    pub size: Size<f64, Logical>,
    pub alpha: f32,
    /// The buffer is stale *and* this entry's policy says not to magnify it —
    /// see [`content_scale`] and [`ContentPolicy`].
    pub cap_content: bool,
}

/// What to do with a stale buffer while a geometry entry animates. The two cases
/// want opposite treatments, so each entry records which it is rather than the
/// render path guessing from whether a request is outstanding.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum ContentPolicy {
    /// A live chase toward a size we just requested: cap the scale at 1 (see
    /// [`content_scale`]).
    Cap,
    /// A seeded hold onto a rect the window has inherited (an adopted stand-in
    /// slot): stretch to fill, since drawing it at its committed size would
    /// leave the window undersized in the corner of the slot.
    Stretch,
}

/// Scale to draw a window's committed buffer at, to fill `visual` on screen.
///
/// Minifying a stale buffer reads fine, but *magnifying* one does not: a fit or
/// fullscreen grows the rect several times over before the client redraws, and
/// stretching the old buffer up to meet it renders the interface hugely
/// oversized for those frames (4.7x for a 400x300 window fitting a 1080p
/// output). So while content is stale the scale is capped at 1: the frame still
/// animates, the stale pixels just sit at their true size until the ack lands.
pub(crate) fn content_scale(
    visual: Size<f64, Logical>,
    committed: Size<f64, Logical>,
    cap_content: bool,
) -> (f64, f64) {
    let (sx, sy) = (
        visual.w / committed.w.max(1.0),
        visual.h / committed.h.max(1.0),
    );
    if cap_content {
        (sx.min(1.0), sy.min(1.0))
    } else {
        (sx, sy)
    }
}

/// Chrome opacity partway through a geometry leg: `from` is what the picture the
/// leg started on wore, `live` what the window wears now, `travelled` how far the
/// leg has come. Fullscreen is the only transition where the two ends differ, so
/// every other window gets a constant out of this.
pub(crate) fn chrome_alpha(from: f32, live: f32, travelled: f32) -> f32 {
    from + (live - from) * travelled
}

/// How large a window opening is drawn, `p` through its fade: from `amplitude`
/// of its rect up to the whole of it.
fn open_scale(amplitude: f64, p: f64) -> f64 {
    amplitude + (1.0 - amplitude) * p
}

/// How opaque a window opening is, `p` through its fade. `1 - (1-p)²`: rises
/// fast so the window isn't translucent through most of the grow-in, then
/// smoothly asymptotes to full opacity at p=1 — eased, with no saturation
/// corner.
fn open_alpha(p: f64) -> f32 {
    (1.0 - (1.0 - p) * (1.0 - p)) as f32
}

/// `rect` scaled about its own centre — the open fade's grow-in, shared by the
/// standalone open entry and a geometry entry that took one over.
fn scaled_about_centre(rect: Rectangle<f64, Logical>, scale: f64) -> Rectangle<f64, Logical> {
    let size = rect.size.upscale(scale);
    Rectangle::new(
        rect.loc + Point::from(((rect.size.w - size.w) / 2.0, (rect.size.h - size.h) / 2.0)),
        size,
    )
}

/// Coordinate space a geometry chase runs in. Canvas entries render through the
/// camera transform; a pinned window's entry is `Screen`, chasing its pin's
/// screen position under zoom 1 (a canvas chase would mis-size at zoom≠1 and
/// never settle during pans, since its stage location is rewritten every tick).
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum AnimSpace {
    Canvas,
    Screen(String),
}

/// Which fullscreen transition a geometry leg is, if any. Besides gating the
/// "visually fullscreen" report, this is how a leg knows what the picture it
/// starts from looked like: stage fullscreen and pin membership both flip when
/// the action runs, so by the time the leg is armed they already describe the
/// destination and the role is the only witness of the side it came from.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum GeometryRole {
    Normal,
    /// Growing into fullscreen. Entering unpins, so the leg carries whether the
    /// picture it starts from was a pinned one.
    FullscreenEntry {
        was_pinned: bool,
    },
    /// Shrinking out of fullscreen on this output. The stage entry is gone by
    /// now, so this name is all that ties the picture still covering that output
    /// to the output it covers.
    FullscreenExit {
        output: String,
    },
}

/// The output a frozen picture hides, and the viewport it hides it under. While
/// one is frozen its output has to keep hiding everything underneath. Purely
/// about coverage — what the picture is drawn *as* is [`FrozenPicture::bare`].
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct FullscreenCover {
    pub output: String,
    /// The camera and zoom the picture was frozen under, for a canvas-space
    /// picture: it covers its output only while the output still shows this
    /// view, and a camera move mid-freeze — a pan gesture, its momentum, a
    /// navigation action — slides it off the very output it claims to be hiding,
    /// leaving the cull to draw black. `None` for a screen-space (re-pinned)
    /// picture, which keeps covering the output whatever the camera does.
    pub view: Option<(Point<f64, Logical>, f64)>,
}

/// What the picture a freeze holds on screen was drawn as. Stamped when a freeze
/// is armed from an unfrozen state and held for as long as that picture is: the
/// stage flips membership at the action, a third of a second before the client
/// redraws into it, so reading any of this live would redress — or restack, or
/// uncover — a motionless pre-action frame.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct FrozenPicture {
    /// The output this picture still hides, if it hides one — coverage, and
    /// nothing else. Deliberately not the witness for [`Self::bare`] as well:
    /// the two answer different questions about the same frame and each has
    /// cases where only one holds. A translucent fullscreen picture hides
    /// nothing yet wears no chrome; a picture whose rect has been reseeded
    /// elsewhere hides nothing either, and is still whatever it was drawn as.
    pub fullscreen_on: Option<FullscreenCover>,
    /// Drawn with no compositor chrome at all — the fullscreen look. What the
    /// chrome hand-over ramps *from*.
    pub bare: bool,
    /// Marked pinned on its title bar, and drawn in the screen-pinned z-bucket
    /// — except on an output some picture is covering, where see
    /// [`crate::state::DriftWm::draws_pinned_on`].
    pub pinned: bool,
    /// There is no earlier picture at all: this entry took over an open fade
    /// that had never been drawn, so the fields above describe nothing that was
    /// ever on screen. Held until some later action stamps a real picture,
    /// rather than clearing when the fade lands — the leg is the same one, and
    /// switching the chrome hand-over on halfway through it would fade a border
    /// ring and shadow in and back out over a window already two-thirds grown.
    pub undrawn: bool,
}

/// One entry's output-scoping data: its id, and `Some((space, visual rect))`
/// for a geometry chase (`None` for an open entry — the caller uses the live
/// stage rect).
pub(crate) type ScopingEntry = (ElementId, Option<(AnimSpace, Rectangle<f64, Logical>)>);

// A geometry entry carries three rects, two deadlines and two output names, so it
// dwarfs the open variant. Boxing it would put an allocation on every resize to
// save a couple of hundred bytes across the handful of entries that are ever live
// at once — the map is per-animating-window, not per-window.
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
enum AnimationKind {
    Open {
        progress: f64,
    },
    Geometry {
        /// Last computed rect — what the renderer draws, and the seed for the
        /// next leg when the target moves.
        visual: Rectangle<f64, Logical>,
        /// Start of the current leg; the visual is `lerp(from, target, progress)`.
        from: Rectangle<f64, Logical>,
        /// The target the current leg aims at, so a moved target is detectable.
        leg_target: Rectangle<f64, Logical>,
        progress: f64,
        space: AnimSpace,
        requested_size: Option<Size<i32, Logical>>,
        /// We have configured a size the client has not committed yet, so its
        /// buffer does not match the rect being animated. A property of the
        /// buffer, not of the request: it outlives the hold deadline (which drops
        /// the request without any commit having landed) and survives a
        /// position-only retarget, so those legs stay capped instead of popping.
        /// Only an actual commit clears it.
        buffer_stale: bool,
        content_policy: ContentPolicy,
        /// Committed size last observed, so a commit that changes size to
        /// anything other than the request reads as "client chose its own".
        last_committed_size: Size<i32, Logical>,
        /// Set on first reaching the endpoint while the request is still
        /// outstanding; releases the hold once it passes. Belongs to the request,
        /// so a position-only retarget carries it rather than re-opening it.
        hold_deadline: Option<Instant>,
        /// The pre-leg freeze. Deliberately its own state rather than derived
        /// from `requested_size`: the degrade path keeps the request (the chase
        /// target comes from it) while `p` advances, so a predicate would
        /// contradict the degrade.
        start_hold: StartHold,
        /// How the picture the freeze holds on screen was drawn — see
        /// [`FrozenPicture`].
        picture: FrozenPicture,
        /// How far the chrome has come from what `picture` wore toward what the
        /// live window wears. Its own accumulator for the same reason
        /// `open_fade` is one, and read the same way: it belongs to the picture,
        /// so it is reset only where that picture is restated. Riding `progress`
        /// instead re-fades chrome that has already gone every time a retarget
        /// or a moved target re-seeds the leg — a nudge mid-shrink, a cluster
        /// shift, the endpoint hold expiring.
        chrome_travelled: f64,
        /// Bumped by every request-carrying (re)start. Stamps the captured old
        /// content so a stale capture can never be paired with a newer leg.
        generation: u64,
        role: GeometryRole,
        /// A view move parked on this entry until its freeze releases, so the
        /// camera and the window start together. Only stored, cleared and handed
        /// back here — what it means is the caller's business.
        pending_view: Option<PendingView>,
        /// This entry does not advance until the entry it names has released its
        /// start freeze — a cluster member pushed by a resize waits for the
        /// window pushing it, so the two move as one. Resolved at tick time (the
        /// named entry need not exist yet when this is set) and dropped as soon
        /// as the wait no longer resolves.
        waits_for: Option<ElementId>,
        /// Progress of an open fade this chase inherited from the open entry it
        /// replaced, so a window still arriving keeps arriving instead of
        /// popping to full opacity. One never drawn is also reseeded at the
        /// destination rect, so a window that maps straight into fullscreen or
        /// fit fades in there rather than at the placement rect it never showed
        /// (see [`FrozenPicture::undrawn`] for what else that case changes).
        ///
        /// Its own accumulator, deliberately not an alias for `progress`: the
        /// `moved` branch re-seeds `progress` on every retarget, so folding the
        /// two together would restart the fade whenever the chase target moves.
        open_fade: Option<f64>,
    },
}

struct WindowAnimation {
    kind: AnimationKind,
}

#[derive(Default)]
pub(crate) struct WindowAnimations {
    animations: HashMap<ElementId, WindowAnimation>,
    /// Monotonic across all entries; see `Geometry::generation`.
    generation: u64,
}

impl WindowAnimations {
    pub fn len(&self) -> usize {
        self.animations.len()
    }

    pub fn is_active(&self) -> bool {
        !self.animations.is_empty()
    }

    pub fn remove(&mut self, id: ElementId) {
        self.animations.remove(&id);
    }

    /// Drop every entry whose id no longer resolves on the stage (crash paths,
    /// and the fixture baseline draining without a tick source).
    pub fn retain_ids(&mut self, mut resolves: impl FnMut(ElementId) -> bool) {
        self.animations.retain(|id, _| resolves(*id));
    }

    pub fn start_open(&mut self, id: ElementId) {
        self.animations.insert(
            id,
            WindowAnimation {
                kind: AnimationKind::Open { progress: 0.0 },
            },
        );
    }

    /// Begin (or retarget) a geometry chase. Retargeting an existing geometry
    /// entry normally keeps its current visual (only the request/role/space
    /// change) so same-space interruptions stay continuous — but a
    /// `replace_visual` caller (fullscreen enter/exit converts coordinate
    /// frames) or a change of chase space overwrites the visual with `seed`, or
    /// a canvas rect would linger where a screen rect belongs (and vice versa).
    ///
    /// `requested_size` is `Some` only for a resize the window has yet to make
    /// — the caller drops a request the window already satisfies, so that one
    /// decision also governs the render-side capture it discards.
    #[allow(clippy::too_many_arguments)]
    pub fn start_geometry(
        &mut self,
        id: ElementId,
        seed: Rectangle<f64, Logical>,
        space: AnimSpace,
        requested_size: Option<Size<i32, Logical>>,
        committed_size: Size<i32, Logical>,
        role: GeometryRole,
        replace_visual: bool,
        content_policy: ContentPolicy,
        picture: FrozenPicture,
        waits_for: Option<ElementId>,
        open_fade: Option<f64>,
    ) {
        if let Some(WindowAnimation {
            kind:
                AnimationKind::Geometry {
                    visual,
                    from,
                    leg_target,
                    progress,
                    space: entry_space,
                    requested_size: entry_request,
                    role: entry_role,
                    hold_deadline,
                    last_committed_size,
                    buffer_stale,
                    content_policy: entry_policy,
                    start_hold,
                    picture: entry_picture,
                    chrome_travelled,
                    generation: entry_generation,
                    pending_view,
                    waits_for: entry_waits_for,
                    // Never written by a retarget — see the request-carrying
                    // branch below for why it has to outlive one.
                    open_fade: _,
                },
        }) = self.animations.get_mut(&id)
        {
            if replace_visual || *entry_space != space {
                *visual = seed;
                // A frozen picture hides an output by being drawn across it, so
                // rewriting the rect it is drawn at — or reading that rect in
                // another coordinate frame — ends the coverage. Keeping the claim
                // culls the scene on a monitor nothing is hiding any more, which
                // draws black. Coverage only: what the picture is drawn *as* has
                // not changed, and restating that here would redress a frame that
                // has not moved.
                entry_picture.fullscreen_on = None;
            }
            // A retarget always starts a fresh leg from wherever the visual is,
            // so the new leg takes a full (distance-independent) duration.
            *from = *visual;
            *leg_target = *visual;
            *progress = 0.0;
            *entry_space = space;
            *entry_role = role;
            *last_committed_size = committed_size;
            // A member pushed again by a fresh cluster shift waits on the new
            // primary. A retarget that names nobody leaves an existing wait
            // alone: nudging a pushed neighbour mid-freeze does nothing until
            // the freeze releases, which is right — it is still the thing being
            // pushed.
            if waits_for.is_some() {
                *entry_waits_for = waits_for;
            }
            // A position-only retarget is the same wait, moving: it leaves the
            // outstanding request, buffer staleness, content policy, both holds'
            // remaining budgets and the frozen picture's chrome stamp untouched, so
            // a nudged window mid-resize keeps holding (capped) and a nudged
            // adopted window keeps filling its slot. Refreshing a budget instead
            // would let a held nudge outrun either deadline and hold the window
            // indefinitely. Only a retarget carrying a size request restates
            // these — a new request makes the buffer stale by definition, brings
            // its own policy, and legitimately re-opens both budgets.
            if requested_size.is_some() {
                *entry_request = requested_size;
                *buffer_stale = true;
                *entry_policy = content_policy;
                *hold_deadline = None;
                // A new request means a new action owns this window's transition,
                // and it brings its own view policy — the parked move belongs to
                // the one it just superseded.
                *pending_view = None;
                // An open fade deliberately survives this: fullscreening a
                // window and toggling it back off before the client acks
                // retargets the same entry, and dropping the fade there would
                // pop a window still arriving to full opacity.
                // A frozen picture keeps the dress it was stamped with: nothing
                // but the client's redraw changes what is on screen, and that
                // releases the freeze. Restating it from the interrupting
                // action's role would dress a motionless frame for a side it is
                // not showing (a fit during a fullscreen exit's freeze would pop
                // chrome onto a still-fullscreen picture). What that picture
                // *covers* is a separate question, answered above.
                if !start_hold.is_held() {
                    *entry_picture = picture;
                    *chrome_travelled = 0.0;
                }
                // A brand new resize: freeze again from wherever the visual is,
                // and invalidate any content captured for the previous request.
                self.generation += 1;
                *entry_generation = self.generation;
                *start_hold = if content_policy == ContentPolicy::Cap {
                    StartHold::Armed
                } else {
                    StartHold::Off
                };
            }
            return;
        }
        if requested_size.is_some() {
            self.generation += 1;
        }
        let generation = self.generation;
        self.animations.insert(
            id,
            WindowAnimation {
                kind: AnimationKind::Geometry {
                    visual: seed,
                    from: seed,
                    leg_target: seed,
                    progress: 0.0,
                    space,
                    requested_size,
                    buffer_stale: requested_size.is_some(),
                    content_policy,
                    last_committed_size: committed_size,
                    hold_deadline: None,
                    start_hold: if requested_size.is_some() && content_policy == ContentPolicy::Cap
                    {
                        StartHold::Armed
                    } else {
                        StartHold::Off
                    },
                    picture,
                    chrome_travelled: 0.0,
                    generation,
                    role,
                    pending_view: None,
                    waits_for,
                    open_fade,
                },
            },
        );
    }

    /// `id` holds an open fade that has never been drawn — mapped this commit
    /// and not ticked since. A geometry chase armed in that same commit takes
    /// the fade over instead of destroying it.
    pub fn open_unshown(&self, id: ElementId) -> bool {
        matches!(
            self.animations.get(&id),
            Some(WindowAnimation {
                kind: AnimationKind::Open { progress }
            }) if *progress == 0.0
        )
    }

    /// Stop `id` waiting to be pushed by anyone. Called for a window that has
    /// acquired an action of its own, judged on what its caller asked for
    /// rather than on the request that survives the visible-resize threshold —
    /// a request too small to freeze for is still this window's own.
    pub fn clear_waits_for(&mut self, id: ElementId) {
        if let Some(WindowAnimation {
            kind: AnimationKind::Geometry { waits_for, .. },
        }) = self.animations.get_mut(&id)
        {
            *waits_for = None;
        }
    }

    /// Progress of the open fade `id` is playing, if any — an open entry's own
    /// fade, or one a geometry chase took over from it.
    pub fn open_progress(&self, id: ElementId) -> Option<f64> {
        match self.animations.get(&id) {
            Some(WindowAnimation {
                kind: AnimationKind::Open { progress },
            }) => Some(*progress),
            Some(WindowAnimation {
                kind: AnimationKind::Geometry { open_fade, .. },
            }) => *open_fade,
            None => None,
        }
    }

    /// Whether `id`'s geometry entry is playing an open fade.
    pub fn has_open_fade(&self, id: ElementId) -> bool {
        matches!(
            self.animations.get(&id),
            Some(WindowAnimation {
                kind: AnimationKind::Geometry {
                    open_fade: Some(_),
                    ..
                }
            })
        )
    }

    /// Park a view move on `id`'s geometry entry, to be handed back when its
    /// freeze releases. No-op when `id` has no geometry entry — there is then no
    /// freeze to wait on, and the caller applies the move itself.
    pub fn stage_pending_view(&mut self, id: ElementId, view: PendingView) {
        if let Some(WindowAnimation {
            kind: AnimationKind::Geometry { pending_view, .. },
        }) = self.animations.get_mut(&id)
        {
            *pending_view = Some(view);
        }
    }

    /// Drop every view move parked for `output`. A parked move is a per-output
    /// effect held on per-entry state, so the action that supersedes one has to
    /// sweep it itself: two fits inside one freeze park identical viewports on
    /// two entries, and at release time nothing can tell the stale payload from
    /// the live one — whichever freeze happens to release first would win.
    pub fn drop_pending_views_on(&mut self, output: &str) {
        for animation in self.animations.values_mut() {
            if let AnimationKind::Geometry { pending_view, .. } = &mut animation.kind
                && pending_view.as_ref().is_some_and(|v| v.output == output)
            {
                *pending_view = None;
            }
        }
    }

    /// Take the view move parked on `id`, if any.
    pub fn take_pending_view(&mut self, id: ElementId) -> Option<PendingView> {
        match self.animations.get_mut(&id) {
            Some(WindowAnimation {
                kind: AnimationKind::Geometry { pending_view, .. },
            }) => pending_view.take(),
            _ => None,
        }
    }

    /// Whether `id` is frozen before its resize leg. The pre-commit hook
    /// refreshes its captured old content off this.
    pub fn start_held(&self, id: ElementId) -> bool {
        matches!(
            self.animations.get(&id),
            Some(WindowAnimation {
                kind: AnimationKind::Geometry { start_hold, .. }
            }) if start_hold.is_held()
        )
    }

    /// Whether the picture `id`'s freeze is holding on screen is a pinned one —
    /// `None` when nothing is frozen and the live stage answer applies.
    pub fn frozen_pinned(&self, id: ElementId) -> Option<bool> {
        match self.animations.get(&id) {
            Some(WindowAnimation {
                kind:
                    AnimationKind::Geometry {
                        start_hold,
                        picture,
                        ..
                    },
            }) if start_hold.is_held() => Some(picture.pinned),
            _ => None,
        }
    }

    /// The chrome opacity `id`'s picture started from, and how far the hand-over
    /// to whatever the *live* window wears has come. `None` when no geometry
    /// entry governs it and the live answer stands alone.
    ///
    /// Interpolating avoids a chrome pop: without it, bar, border and shadow
    /// would blink out on the frame the freeze releases, while the window is
    /// still small with its whole leg left to grow. It also makes the ramp
    /// direction fall out of the two pictures rather than out of the role,
    /// which a retarget can restate mid-transition. Only alpha ramps — border
    /// *width* stays full throughout, since shrinking it reads as jarring.
    pub fn chrome_ramp(&self, id: ElementId) -> Option<(f32, f32)> {
        let Some(WindowAnimation {
            kind:
                AnimationKind::Geometry {
                    picture,
                    chrome_travelled,
                    ..
                },
        }) = self.animations.get(&id)
        else {
            return None;
        };
        // A fade taken over before it was ever drawn has no earlier picture to
        // hand the chrome over *from*, so the live answer (bare for fullscreen,
        // chrome for a fit) is the whole truth. A fade the user has already seen
        // is the opposite case: that picture wore chrome, at a rect this leg
        // starts from, and blinking it out is exactly what the ramp exists to
        // prevent.
        if picture.undrawn {
            return None;
        }
        let from = if picture.bare { 0.0 } else { 1.0 };
        let travelled = chrome_travelled.clamp(0.0, 1.0) as f32;
        Some((from, travelled))
    }

    /// The entry frozen on a fullscreen picture covering `output`, which is
    /// showing `camera`/`zoom`. Nothing has moved yet, so that picture is still
    /// what the output shows — whatever the stage now says, and whatever the leg
    /// has since been retargeted to — for as long as the view it was stamped
    /// under holds. Compared exactly: a view that drifted by a hair still reports
    /// no cover, and the only cost of that is drawing a scene the picture hides.
    pub fn frozen_fullscreen_on(
        &self,
        output: &str,
        camera: Point<f64, Logical>,
        zoom: f64,
    ) -> Option<ElementId> {
        self.animations.iter().find_map(|(id, a)| match &a.kind {
            AnimationKind::Geometry {
                start_hold,
                picture,
                ..
            } if start_hold.is_held()
                && picture.fullscreen_on.as_ref().is_some_and(|cover| {
                    cover.output == output
                        && cover.view.is_none_or(|(c, z)| c == camera && z == zoom)
                }) =>
            {
                Some(*id)
            }
            _ => None,
        })
    }

    /// `id`'s freeze and the entry it is waiting on, if it has a geometry entry.
    fn freeze_state(&self, id: ElementId) -> Option<(StartHold, Option<ElementId>)> {
        match self.animations.get(&id) {
            Some(WindowAnimation {
                kind:
                    AnimationKind::Geometry {
                        start_hold,
                        waits_for,
                        ..
                    },
            }) => Some((*start_hold, *waits_for)),
            _ => None,
        }
    }

    /// Whether `id` is frozen and will still be frozen after a tick at `now` —
    /// i.e. it will paint the same picture again. The tick that lets a budget
    /// expire reports false, since that one does move the window. A follower
    /// parked on the entry it waits for counts the same: it repaints its seed,
    /// and answering otherwise would mark its output for redraw every frame of
    /// a freeze the primary marks only its own output for.
    ///
    /// The wait is resolved exactly one hop, deliberately not recursively —
    /// and that, rather than any property of the entries, is what makes this
    /// terminate. Chains are constructible: a position-only retarget can name a
    /// primary on an entry that is itself frozen and waiting, so following
    /// `waits_for` transitively would be a hang waiting for a cycle to be built.
    /// One hop is also all the answer is worth: an entry two removes away is
    /// somebody else's lockstep, not this one's.
    pub fn frozen_at(&self, id: ElementId, now: Instant) -> bool {
        let Some((start_hold, waits_for)) = self.freeze_state(id) else {
            return false;
        };
        if start_hold.holds_at(now) {
            return true;
        }
        waits_for.is_some_and(|other| {
            self.freeze_state(other)
                .is_some_and(|(hold, _)| hold.holds_at(now))
        })
    }

    /// Whether any entry is frozen. The per-commit capture hook probes this
    /// before the per-surface work that only a frozen window needs.
    pub fn any_start_held(&self) -> bool {
        self.animations.values().any(|a| {
            matches!(&a.kind, AnimationKind::Geometry { start_hold, .. } if start_hold.is_held())
        })
    }

    /// Capture generation of `id`'s current request, for pairing stashed content.
    pub fn generation_of(&self, id: ElementId) -> Option<u64> {
        match self.animations.get(&id) {
            Some(WindowAnimation {
                kind: AnimationKind::Geometry { generation, .. },
            }) => Some(*generation),
            _ => None,
        }
    }

    /// The space a geometry entry's visual rect is expressed in. Anyone reading
    /// that rect needs this too: the entry's space and the window's live pin
    /// membership can disagree, and reading canvas coords as screen px is a
    /// wildly wrong answer rather than a slightly stale one.
    pub fn geometry_space(&self, id: ElementId) -> Option<AnimSpace> {
        match self.animations.get(&id) {
            Some(WindowAnimation {
                kind: AnimationKind::Geometry { space, .. },
            }) => Some(space.clone()),
            _ => None,
        }
    }

    /// The current visual rect of a geometry entry in its own space, if any.
    pub fn geometry_visual_rect(&self, id: ElementId) -> Option<Rectangle<f64, Logical>> {
        match self.animations.get(&id) {
            Some(WindowAnimation {
                kind: AnimationKind::Geometry { visual, .. },
            }) => Some(*visual),
            _ => None,
        }
    }

    /// The canvas rect a geometry entry is currently drawn at, for render
    /// culling. `None` for a `Screen` entry — its rect lives in screen space,
    /// and the pin that put it there already scopes it to one output.
    pub fn canvas_visual_rect(&self, id: ElementId) -> Option<Rectangle<f64, Logical>> {
        match self.animations.get(&id) {
            Some(WindowAnimation {
                kind:
                    AnimationKind::Geometry {
                        visual,
                        space: AnimSpace::Canvas,
                        ..
                    },
            }) => Some(*visual),
            _ => None,
        }
    }

    /// A geometry entry with the fullscreen-entry role is still playing. Once it
    /// prunes the output counts as visually fullscreen.
    pub fn fullscreen_entry_active(&self, id: ElementId) -> bool {
        matches!(
            self.animations.get(&id),
            Some(WindowAnimation {
                kind: AnimationKind::Geometry {
                    role: GeometryRole::FullscreenEntry { .. },
                    ..
                }
            })
        )
    }

    /// Per-entry data for output scoping: `Some((space, visual))` for a
    /// geometry chase (its rect lives in that space), `None` for an open entry
    /// (the caller uses the window's live stage rect).
    pub fn scoping_entries(&self) -> Vec<ScopingEntry> {
        self.animations
            .iter()
            .map(|(id, a)| match &a.kind {
                AnimationKind::Open { .. } => (*id, None),
                AnimationKind::Geometry { visual, space, .. } => {
                    (*id, Some((space.clone(), *visual)))
                }
            })
            .collect()
    }

    /// Render-time lookup: the animated stand-in for `id`, given the window's
    /// live target rect (canvas rect for a normal window, screen rect for a
    /// pinned one) and the configured open/close scale amplitude. Returns the
    /// identity visual when nothing is animating.
    pub fn animated_visual(
        &self,
        id: ElementId,
        target_loc: Point<f64, Logical>,
        target_size: Size<f64, Logical>,
        amplitude: f64,
    ) -> AnimatedVisual {
        let Some(animation) = self.animations.get(&id) else {
            return AnimatedVisual {
                loc: target_loc,
                size: target_size,
                alpha: 1.0,
                cap_content: false,
            };
        };
        match &animation.kind {
            AnimationKind::Open { progress } => {
                let p = progress.clamp(0.0, 1.0);
                let rect = scaled_about_centre(
                    Rectangle::new(target_loc, target_size),
                    open_scale(amplitude, p),
                );
                AnimatedVisual {
                    loc: rect.loc,
                    size: rect.size,
                    alpha: open_alpha(p),
                    cap_content: false,
                }
            }
            AnimationKind::Geometry {
                visual,
                buffer_stale,
                content_policy,
                start_hold,
                open_fade,
                ..
            } => {
                // A chase carrying an open fade draws its own rect scaled and
                // faded, so the window arrives at its destination instead of
                // popping in at the placement rect it never showed.
                let (rect, alpha) = match open_fade {
                    Some(fade) => {
                        let p = fade.clamp(0.0, 1.0);
                        (
                            scaled_about_centre(*visual, open_scale(amplitude, p)),
                            open_alpha(p),
                        )
                    }
                    None => (*visual, 1.0),
                };
                AnimatedVisual {
                    loc: rect.loc,
                    size: rect.size,
                    alpha,
                    // A frozen window renders at its seed ratio, uncapped: the
                    // seed reproduces exactly what was on screen before the
                    // action, which for a frame-converted seed (fullscreen at
                    // zoom) is not 1:1, and capping would visibly shrink the
                    // "frozen" window. The cap only applies once a degrade
                    // starts a leg running with stale content.
                    cap_content: !start_hold.is_held()
                        && *buffer_stale
                        && *content_policy == ContentPolicy::Cap,
                }
            }
        }
    }

    /// Resolve the outstanding request on a commit of the animated window: a
    /// clean ack (committed == request) or the client picking its own size
    /// both bend the chase to live; a size-unchanged commit does nothing.
    ///
    /// Returns the generation of a request whose *start hold* this commit
    /// released — the one moment old and new content both exist, so the caller
    /// can pair the stashed old picture with this leg and crossfade it.
    pub fn on_window_commit(
        &mut self,
        id: ElementId,
        committed_size: Size<i32, Logical>,
    ) -> Option<u64> {
        let Some(WindowAnimation {
            kind:
                AnimationKind::Geometry {
                    requested_size,
                    last_committed_size,
                    buffer_stale,
                    start_hold,
                    generation,
                    ..
                },
        }) = self.animations.get_mut(&id)
        else {
            return None;
        };
        let Some(request) = *requested_size else {
            // No request outstanding — but a commit that changes size is still
            // the resolution arriving (late, after a deadline release dropped
            // the request), so it clears staleness. A same-size redraw does
            // not: the buffer still doesn't match the rect.
            if committed_size != *last_committed_size {
                *buffer_stale = false;
            }
            *last_committed_size = committed_size;
            return None;
        };
        let mut released = None;
        if committed_size == request || committed_size != *last_committed_size {
            *requested_size = None;
            // Only an actual commit clears staleness.
            *buffer_stale = false;
            // The redraw the freeze was waiting for: release it so the leg
            // can play with real content on both sides.
            released = start_hold.is_held().then_some(*generation);
            *start_hold = StartHold::Off;
        }
        *last_committed_size = committed_size;
        released
    }

    /// Advance one entry by `frame_factor`. The chase target is `target_loc`
    /// plus the requested size when one is outstanding, else `live_size` — so
    /// the visual stretches toward the requested rect immediately (one phase).
    /// On reaching a still-requested endpoint it holds the stretched rect until
    /// the client commits or the deadline fires, then the chase bends to live.
    /// `eligible` is false when the entry's rect intersects no drawable output;
    /// such an entry completes instantly. Returns whether to keep the entry.
    pub fn tick_entry(
        &mut self,
        id: ElementId,
        target_loc: Point<f64, Logical>,
        live_size: Size<f64, Logical>,
        frame_factor: f64,
        now: Instant,
        eligible: bool,
    ) -> bool {
        // Resolved before the entry is borrowed, since the answer lives on a
        // different entry. Asked as "still frozen after a tick at `now`" rather
        // than "frozen": tick order over the map is arbitrary, so a predicate
        // that ignores `now` would release followers a frame apart depending on
        // whether they happened to be ticked before or after the primary.
        let waiting = match self.animations.get(&id) {
            Some(WindowAnimation {
                kind:
                    AnimationKind::Geometry {
                        waits_for: Some(other),
                        ..
                    },
            }) => self.frozen_at(*other, now),
            _ => false,
        };
        let Some(animation) = self.animations.get_mut(&id) else {
            return false;
        };
        match &mut animation.kind {
            AnimationKind::Open { progress } => {
                if !eligible {
                    return false;
                }
                *progress += (1.0 - *progress) * frame_factor;
                1.0 - *progress > PROGRESS_DONE_EPSILON
            }
            AnimationKind::Geometry {
                visual,
                from,
                leg_target,
                progress,
                requested_size,
                hold_deadline,
                start_hold,
                waits_for,
                open_fade,
                chrome_travelled,
                ..
            } => {
                if !eligible {
                    return false;
                }
                // Frozen: nothing advances until the client redraws (handled in
                // `on_window_commit`) or the budget runs out, which degrades to
                // animating with capped stale content. The deadline anchors on
                // the first tick that reaches it — ahead of the wait below, so
                // an entry that is both frozen on its own resize and following
                // another runs the two budgets concurrently instead of end to
                // end (twice the freeze, motionless, with its capture and its
                // parked view held for all of it).
                if *start_hold == StartHold::Armed {
                    *start_hold = StartHold::Until(now + MAX_START_HOLD);
                }
                let frozen = start_hold.holds_at(now);
                // Only this entry's own freeze pins its arrival at zero: there
                // is no picture to fade in over yet. Waiting on someone else's
                // freeze must not, or a just-launched window is held completely
                // invisible for the length of it. Cleared on landing so the
                // resize crossfade, suppressed while a window is still fading
                // in, comes back for whatever leg follows on this entry.
                if !frozen && let Some(fade) = *open_fade {
                    let fade = fade + (1.0 - fade) * frame_factor;
                    *open_fade = (1.0 - fade > PROGRESS_DONE_EPSILON).then_some(fade);
                }
                // Parked on the seed while the entry pushing this one is frozen,
                // so the two start on the same tick and travel in lockstep.
                if waiting {
                    return true;
                }
                // The wait is dropped the moment it stops resolving — released,
                // degraded, never armed, or the entry gone — and never retaken.
                *waits_for = None;
                if frozen {
                    return true;
                }
                // Past the deadline (or never held): the leg runs from here.
                *start_hold = StartHold::Off;
                // The chrome travels with the leg but keeps its own clock: it is
                // handing one picture over to another, and neither a retarget nor
                // a moved target changes which two those are.
                *chrome_travelled += (1.0 - *chrome_travelled) * frame_factor;
                let target = Rectangle::new(
                    target_loc,
                    requested_size.map(|s| s.to_f64()).unwrap_or(live_size),
                );
                // A moved target (commit resolution, settle recenter, adopt move,
                // deadline release) starts a fresh leg from where the visual is —
                // continuous, and the new leg takes a full duration rather than
                // teleporting by the target delta.
                let moved = (target.loc.x - leg_target.loc.x).abs() > TARGET_MOVED_EPSILON
                    || (target.loc.y - leg_target.loc.y).abs() > TARGET_MOVED_EPSILON
                    || (target.size.w - leg_target.size.w).abs() > TARGET_MOVED_EPSILON
                    || (target.size.h - leg_target.size.h).abs() > TARGET_MOVED_EPSILON;
                if moved {
                    *from = *visual;
                    *leg_target = target;
                    *progress = 0.0;
                }
                *progress += (1.0 - *progress) * frame_factor;
                let p = progress.clamp(0.0, 1.0);
                // Lerp to the result directly (never via a delta): a shrink would
                // build a negative-component `Size`, which panics in debug.
                visual.loc = Point::from((
                    from.loc.x + (target.loc.x - from.loc.x) * p,
                    from.loc.y + (target.loc.y - from.loc.y) * p,
                ));
                visual.size = Size::from((
                    (from.size.w + (target.size.w - from.size.w) * p).max(0.0),
                    (from.size.h + (target.size.h - from.size.h) * p).max(0.0),
                ));

                if 1.0 - *progress > PROGRESS_DONE_EPSILON {
                    return true;
                }
                if requested_size.is_none() {
                    return false;
                }
                // Endpoint hold: pin the stretched (requested) rect until the
                // client commits, or the deadline (anchored here, at
                // endpoint-reach) releases it — clearing the request moves the
                // target, which re-seeds a final leg back to the live size.
                // `buffer_stale` deliberately survives that release: no commit
                // landed, so the release leg must stay capped rather than
                // magnifying the old buffer on its way back down.
                *visual = target;
                match hold_deadline {
                    Some(deadline) if now >= *deadline => {
                        *requested_size = None;
                        true
                    }
                    Some(_) => true,
                    None => {
                        *hold_deadline = Some(now + MAX_ENDPOINT_HOLD);
                        true
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod chrome_alpha_tests {
    use super::*;

    /// Chrome opacity for a picture that wears it (windowed) and one that does
    /// not (fullscreen), as the two ends of a leg.
    const WINDOWED: f32 = 1.0;
    const FULLSCREEN: f32 = 0.0;

    /// A fullscreen enter hands the chrome over across the whole grow instead of
    /// blinking it out on the frame the client redraws — when the window is still
    /// small and has its entire leg left to play.
    #[test]
    fn a_fullscreen_entry_fades_the_chrome_out_over_the_leg() {
        assert_eq!(chrome_alpha(WINDOWED, FULLSCREEN, 0.0), 1.0);
        assert_eq!(chrome_alpha(WINDOWED, FULLSCREEN, 0.5), 0.5);
        assert_eq!(chrome_alpha(WINDOWED, FULLSCREEN, 1.0), 0.0);
    }

    /// The exit is the same ramp read the other way: the frozen picture is the
    /// bare fullscreen one and the chrome arrives as the window shrinks back.
    #[test]
    fn a_fullscreen_exit_fades_the_chrome_in_over_the_leg() {
        assert_eq!(chrome_alpha(FULLSCREEN, WINDOWED, 0.0), 0.0);
        assert_eq!(chrome_alpha(FULLSCREEN, WINDOWED, 0.5), 0.5);
        assert_eq!(chrome_alpha(FULLSCREEN, WINDOWED, 1.0), 1.0);
    }

    /// Every leg that does not cross the fullscreen boundary — a fit, a fill, a
    /// nudge, a resize of an already-fullscreen window — has both ends equal, so
    /// the ramp is inert and the chrome never flickers.
    #[test]
    fn a_leg_between_like_pictures_never_moves_the_chrome() {
        for travelled in [0.0, 0.25, 0.5, 0.75, 1.0] {
            assert_eq!(chrome_alpha(WINDOWED, WINDOWED, travelled), 1.0);
            assert_eq!(chrome_alpha(FULLSCREEN, FULLSCREEN, travelled), 0.0);
        }
    }
}

#[cfg(test)]
mod content_scale_tests {
    use super::*;

    fn size(w: f64, h: f64) -> Size<f64, Logical> {
        Size::from((w, h))
    }

    /// Growing past the committed buffer is capped at 1 while it is stale — the
    /// magnification that made a fit look like a huge interface.
    #[test]
    fn stale_content_is_never_magnified() {
        let (sx, sy) = content_scale(size(1896.0, 1056.0), size(400.0, 300.0), true);
        assert_eq!((sx, sy), (1.0, 1.0));
    }

    /// Shrinking a stale buffer reads fine, so minification is left alone.
    #[test]
    fn stale_content_still_minifies() {
        let (sx, _) = content_scale(size(200.0, 150.0), size(400.0, 300.0), true);
        assert_eq!(sx, 0.5);
    }

    /// Once the client has acked, the buffer matches the rect and the ratio is
    /// used as-is (the open animation's grow-in relies on this).
    #[test]
    fn fresh_content_scales_freely() {
        let (sx, sy) = content_scale(size(800.0, 600.0), size(400.0, 300.0), false);
        assert_eq!((sx, sy), (2.0, 2.0));
    }
}
