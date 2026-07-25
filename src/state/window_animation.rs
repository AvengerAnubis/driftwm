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

/// The rendered stand-in for a window while an animation is playing: where its
/// content is drawn, at what size, and how opaque. Named apart from the
/// `window_visual_*` family (which reports the pin-aware logical rect).
#[derive(Clone, Copy, Debug)]
pub(crate) struct AnimatedVisual {
    pub loc: Point<f64, Logical>,
    pub size: Size<f64, Logical>,
    pub alpha: f32,
    /// The client has not yet committed the size we asked for, so its buffer
    /// does not match `size`. Callers must not magnify content in this state —
    /// see [`content_scale`].
    pub content_stale: bool,
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
    content_stale: bool,
) -> (f64, f64) {
    let (sx, sy) = (
        visual.w / committed.w.max(1.0),
        visual.h / committed.h.max(1.0),
    );
    if content_stale {
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
        /// Committed size last observed, so a commit that changes size to
        /// anything other than the request reads as "client chose its own".
        last_committed_size: Size<i32, Logical>,
        /// Set on first reaching the endpoint while the request is still
        /// outstanding; releases the hold once it passes.
        hold_deadline: Option<Instant>,
        role: GeometryRole,
    },
}

struct WindowAnimation {
    kind: AnimationKind,
}

#[derive(Default)]
pub(crate) struct WindowAnimations {
    animations: HashMap<ElementId, WindowAnimation>,
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
            *entry_request = requested_size;
            *entry_role = role;
            *hold_deadline = None;
            *last_committed_size = committed_size;
            return;
        }
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
                    last_committed_size: committed_size,
                    hold_deadline: None,
                    role,
                },
            },
        );
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
                content_stale: false,
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
                    content_stale: false,
                }
            }
            AnimationKind::Geometry {
                visual,
                requested_size,
                ..
            } => AnimatedVisual {
                loc: visual.loc,
                size: visual.size,
                alpha: 1.0,
                content_stale: requested_size.is_some(),
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
                    ..
                },
        }) = self.animations.get_mut(&id)
        {
            let Some(request) = *requested_size else {
                *last_committed_size = committed_size;
                return;
            };
            if committed_size == request || committed_size != *last_committed_size {
                *requested_size = None;
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
                ..
            } => {
                if !eligible {
                    return false;
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
