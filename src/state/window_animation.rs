use std::collections::HashMap;
use std::time::{Duration, Instant};

use smithay::utils::{Logical, Point, Rectangle, Size};

use driftwm::stage::ElementId;

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
/// before giving up and animating with stale content. Same bound as the endpoint
/// hold: a client that misses one misses both.
pub(crate) const MAX_START_HOLD: Duration = MAX_ENDPOINT_HOLD;

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

/// Coordinate space a geometry chase runs in. Canvas entries render through the
/// camera transform; a pinned window's entry is `Screen`, chasing its pin's
/// screen position under zoom 1 (a canvas chase would mis-size at zoom≠1 and
/// never settle during pans, since its stage location is rewritten every tick).
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum AnimSpace {
    Canvas,
    Screen(String),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum GeometryRole {
    Normal,
    FullscreenEntry,
}

/// One entry's output-scoping data: its id, and `Some((space, visual rect))`
/// for a geometry chase (`None` for an open entry — the caller uses the live
/// stage rect).
pub(crate) type ScopingEntry = (ElementId, Option<(AnimSpace, Rectangle<f64, Logical>)>);

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
        /// outstanding; releases the hold once it passes.
        hold_deadline: Option<Instant>,
        /// The pre-leg freeze. Deliberately its own state rather than derived
        /// from `requested_size`: the degrade path keeps the request (the chase
        /// target comes from it) while `p` advances, so a predicate would
        /// contradict the degrade.
        start_hold: StartHold,
        /// Bumped by every request-carrying (re)start. Stamps the captured old
        /// content so a stale capture can never be paired with a newer leg.
        generation: u64,
        role: GeometryRole,
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
    ) {
        // A request that already equals the committed size must not ride to the
        // hold deadline — resolve it immediately.
        let requested_size = requested_size.filter(|sz| *sz != committed_size);
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
                    generation: entry_generation,
                },
        }) = self.animations.get_mut(&id)
        {
            if replace_visual || *entry_space != space {
                *visual = seed;
            }
            // A retarget always starts a fresh leg from wherever the visual is,
            // so the new leg takes a full (distance-independent) duration.
            *from = *visual;
            *leg_target = *visual;
            *progress = 0.0;
            *entry_space = space;
            *entry_role = role;
            *hold_deadline = None;
            *last_committed_size = committed_size;
            // A position-only retarget is the same hold, moving: it leaves the
            // outstanding request, the buffer's staleness, and the content policy
            // exactly as they were, so a nudged window mid-resize keeps holding
            // (capped) and a nudged adopted window keeps filling its slot. Only a
            // retarget that carries a size request restates them — a new request
            // makes the buffer stale by definition and brings its own policy.
            if requested_size.is_some() {
                *entry_request = requested_size;
                *buffer_stale = true;
                *entry_policy = content_policy;
                // A brand new resize: freeze again from wherever the visual is,
                // and invalidate any content captured for the previous request.
                self.generation += 1;
                *entry_generation = self.generation;
                *start_hold = if content_policy == ContentPolicy::Cap {
                    StartHold::Armed
                } else {
                    StartHold::Off
                };
            } else if start_hold.is_held() {
                // Still the same freeze, just moving — but re-anchor it, so the
                // budget is measured from the user's latest action.
                *start_hold = StartHold::Armed;
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
                    generation,
                    role,
                },
            },
        );
    }

    /// Whether `id` is frozen before its resize leg. Test-only until the capture
    /// stash lands — the pre-commit hook refreshes the stash off this.
    #[cfg(test)]
    pub fn start_held(&self, id: ElementId) -> bool {
        matches!(
            self.animations.get(&id),
            Some(WindowAnimation {
                kind: AnimationKind::Geometry { start_hold, .. }
            }) if start_hold.is_held()
        )
    }

    /// Capture generation of `id`'s current request, for pairing stashed content.
    /// Test-only until the stash lands and consumes it.
    #[cfg(test)]
    pub fn generation_of(&self, id: ElementId) -> Option<u64> {
        match self.animations.get(&id) {
            Some(WindowAnimation {
                kind: AnimationKind::Geometry { generation, .. },
            }) => Some(*generation),
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

    /// A geometry entry with the fullscreen-entry role is still playing. Once it
    /// prunes the output counts as visually fullscreen.
    pub fn fullscreen_entry_active(&self, id: ElementId) -> bool {
        matches!(
            self.animations.get(&id),
            Some(WindowAnimation {
                kind: AnimationKind::Geometry {
                    role: GeometryRole::FullscreenEntry,
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
        open_scale: f64,
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
                let scale = open_scale + (1.0 - open_scale) * p;
                let size = target_size.upscale(scale);
                let loc = target_loc
                    + Point::from((
                        (target_size.w - size.w) / 2.0,
                        (target_size.h - size.h) / 2.0,
                    ));
                AnimatedVisual {
                    loc,
                    size,
                    // `1 - (1-p)²`: rises fast so the window isn't translucent
                    // through most of the grow-in, then smoothly asymptotes to
                    // full opacity at p=1 — eased, with no saturation corner.
                    alpha: (1.0 - (1.0 - p) * (1.0 - p)) as f32,
                    cap_content: false,
                }
            }
            AnimationKind::Geometry {
                visual,
                buffer_stale,
                content_policy,
                start_hold,
                ..
            } => AnimatedVisual {
                loc: visual.loc,
                size: visual.size,
                alpha: 1.0,
                // A frozen window renders at its seed ratio, uncapped: the seed
                // reproduces exactly what was on screen before the action, which
                // for a frame-converted seed (fullscreen at zoom) is not 1:1.
                // Capping would visibly shrink the "frozen" window. The cap is for
                // a leg that runs with stale content, i.e. after a degrade.
                cap_content: !start_hold.is_held()
                    && *buffer_stale
                    && *content_policy == ContentPolicy::Cap,
            },
        }
    }

    /// Resolve the outstanding request on a commit of the animated window: a
    /// clean ack (committed == request) or the client picking its own size
    /// both bend the chase to live; a size-unchanged commit does nothing.
    pub fn on_window_commit(&mut self, id: ElementId, committed_size: Size<i32, Logical>) {
        if let Some(WindowAnimation {
            kind:
                AnimationKind::Geometry {
                    requested_size,
                    last_committed_size,
                    buffer_stale,
                    start_hold,
                    ..
                },
        }) = self.animations.get_mut(&id)
        {
            let Some(request) = *requested_size else {
                // No request outstanding — but a commit that changes size is still
                // the resolution arriving (late, after a deadline release dropped
                // the request), so it clears staleness. A same-size redraw does
                // not: the buffer still doesn't match the rect.
                if committed_size != *last_committed_size {
                    *buffer_stale = false;
                }
                *last_committed_size = committed_size;
                return;
            };
            if committed_size == request || committed_size != *last_committed_size {
                *requested_size = None;
                // Only an actual commit clears staleness.
                *buffer_stale = false;
                // The redraw the freeze was waiting for: release it so the leg
                // can play with real content on both sides.
                *start_hold = StartHold::Off;
            }
            *last_committed_size = committed_size;
        }
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
                ..
            } => {
                if !eligible {
                    return false;
                }
                // Frozen: nothing advances until the client redraws (handled in
                // `on_window_commit`) or the budget runs out, which degrades to
                // animating with capped stale content.
                match *start_hold {
                    StartHold::Armed => {
                        *start_hold = StartHold::Until(now + MAX_START_HOLD);
                        return true;
                    }
                    StartHold::Until(deadline) if now < deadline => return true,
                    StartHold::Until(_) => *start_hold = StartHold::Off,
                    StartHold::Off => {}
                }
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
