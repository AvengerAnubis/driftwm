use std::time::{Duration, Instant};

use smithay::desktop::Window;
use smithay::input::pointer::CursorImageStatus;
use smithay::reexports::wayland_server::Resource;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Logical, Point, Rectangle, Size};

use driftwm::canvas::{self, CanvasPos};
use driftwm::stage::{ElementId, StageElement};
use smithay::wayland::compositor::{BufferAssignment, SurfaceAttributes, with_states};
use smithay::wayland::seat::WaylandFocus;
use smithay::wayland::shell::wlr_layer::Layer as WlrLayer;

use smithay::output::Output;

use super::window_animation::{AnimSpace, AnimatedVisual, ContentPolicy, GeometryRole};
use super::{DriftWm, FocusTarget, output_state};

impl DriftWm {
    /// Frame-rate independent lerp factor for smooth animations.
    /// Returns how much of the remaining distance to cover this frame.
    fn animation_factor(&self, dt: Duration) -> f64 {
        let base = self.config.camera_speed;
        let dt_secs = dt.as_secs_f64();
        1.0 - (1.0 - base).powf(dt_secs * 60.0)
    }

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

    /// Whether the picture on screen for `window` is a fullscreen one — no
    /// compositor chrome. Normally that is stage membership, but a frozen resize
    /// keeps showing its pre-action frame long after the stage has flipped, and
    /// that frame has to keep the chrome it was drawn with.
    pub(crate) fn chrome_fullscreen(&self, window: &Window) -> bool {
        self.stage
            .id_of(window)
            .and_then(|id| self.window_animations.frozen_chrome_fullscreen(id))
            .unwrap_or_else(|| self.stage.is_fullscreen(window))
    }

    fn window_geometry_grab_active(&self, window: &Window) -> bool {
        window
            .wl_surface()
            .is_some_and(|s| self.window_under_interactive_grab(window, &s))
    }

    /// True if a canvas rect intersects some output that can actually draw it
    /// (live, not DPMS-off). Animations intersecting no such output complete
    /// instantly, so they never wedge the udev idle fast-path.
    pub(crate) fn canvas_rect_drawable(&self, rect: Rectangle<i32, Logical>) -> bool {
        self.space.outputs().any(|o| {
            if self.dpms_off_outputs.contains(o) {
                return false;
            }
            let (camera, zoom) = {
                let os = output_state(o);
                (os.camera, os.zoom)
            };
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
        if self.window_geometry_grab_active(window) {
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

    /// Shared start path for every geometry chase: resolve the id, honor the
    /// interactive-grab guard, instant-complete (skip) when the seed rect
    /// intersects no drawable output, else (re)start the chase. `replace_visual`
    /// forces the seed onto an existing entry — the seeded (fullscreen) callers
    /// convert coordinate frames, so keeping the old visual would jump at zoom≠1.
    #[allow(clippy::too_many_arguments)]
    fn start_geometry_entry(
        &mut self,
        window: &Window,
        seed: Rectangle<f64, Logical>,
        space: AnimSpace,
        requested_size: Option<Size<i32, Logical>>,
        role: GeometryRole,
        replace_visual: bool,
        content_policy: ContentPolicy,
    ) {
        let Some(id) = self.stage.id_of(window) else {
            return;
        };
        if self.window_geometry_grab_active(window) {
            return;
        }
        let eligible = match &space {
            AnimSpace::Screen(name) => self.output_name_drawable(name),
            AnimSpace::Canvas => self.canvas_rect_drawable(seed.to_i32_round()),
        };
        if !eligible {
            return;
        }
        let committed = window.geometry().size;
        // A request the window already satisfies is no request at all: drop it
        // here, once, so the freeze `start_geometry` arms and the capture dropped
        // below can never disagree about whether a resize is starting.
        let requested_size = requested_size.filter(|size| *size != committed);
        // A brand new resize supersedes the last one: its captured content is for
        // a request nobody waits on any more, and a live overlay belongs to a leg
        // that no longer exists.
        if requested_size.is_some() {
            self.drop_resize_crossfade(id);
        }
        // What the picture this leg starts from wore. A fullscreen leg is armed
        // after the stage has already flipped, so its role is the only witness of
        // the side it came from.
        let chrome_fullscreen = match role {
            GeometryRole::FullscreenEntry => false,
            GeometryRole::FullscreenExit => true,
            GeometryRole::Normal => self.stage.is_fullscreen(window),
        };
        self.window_animations.start_geometry(
            id,
            seed,
            space,
            requested_size,
            committed,
            role,
            replace_visual,
            content_policy,
            chrome_fullscreen,
        );
    }

    /// Seed rect for a fresh geometry entry: the window's current animated
    /// visual, so an interruption or an open→geometry hand-off is continuous.
    fn geometry_seed(
        &self,
        id: ElementId,
        loc: Point<i32, Logical>,
        size: Size<i32, Logical>,
    ) -> Rectangle<f64, Logical> {
        let v = self.animated_visual(id, loc.to_f64(), size.to_f64());
        Rectangle::new(v.loc, v.size)
    }

    /// Canvas geometry animation toward a size configure (fill/fit). Must be
    /// called while the stage still holds the pre-action rect; the chase target
    /// is then the new live stage position.
    pub(crate) fn animate_window_geometry(&mut self, window: &Window, to_size: Size<i32, Logical>) {
        let Some(id) = self.stage.id_of(window) else {
            return;
        };
        let old_loc = self.stage.position_of(window).unwrap_or_default();
        let seed = self.geometry_seed(id, old_loc, window.geometry().size);
        self.start_geometry_entry(
            window,
            seed,
            AnimSpace::Canvas,
            Some(to_size),
            GeometryRole::Normal,
            false,
            ContentPolicy::Cap,
        );
    }

    /// Geometry animation with an explicit, frame-converted seed (fullscreen
    /// enter/exit cross the locked-viewport ↔ camera ↔ pin-screen boundary).
    pub(crate) fn begin_geometry_animation_seeded(
        &mut self,
        window: &Window,
        seed: Rectangle<f64, Logical>,
        space: AnimSpace,
        requested_size: Option<Size<i32, Logical>>,
        role: GeometryRole,
        content_policy: ContentPolicy,
    ) {
        self.start_geometry_entry(
            window,
            seed,
            space,
            requested_size,
            role,
            true,
            content_policy,
        );
    }

    /// Position-only canvas animation from `from_loc` (nudge, cluster shift).
    /// The stage already holds the new position; the seed pins the old one.
    pub(crate) fn animate_window_move_from(
        &mut self,
        window: &Window,
        from_loc: Point<i32, Logical>,
    ) {
        let Some(id) = self.stage.id_of(window) else {
            return;
        };
        let size = window.geometry().size;
        // Keep an in-flight entry's visual; otherwise seed at the old position.
        let seed = self
            .window_animations
            .geometry_visual_rect(id)
            .unwrap_or_else(|| {
                let v = self.animated_visual(id, from_loc.to_f64(), size.to_f64());
                Rectangle::new(v.loc, v.size)
            });
        self.start_geometry_entry(
            window,
            seed,
            AnimSpace::Canvas,
            None,
            GeometryRole::Normal,
            false,
            ContentPolicy::Cap,
        );
    }

    /// Whether `output` is occluded by a *settled* fullscreen window. A
    /// fullscreen-entry transition keeps the previous scene visible until the
    /// window reaches the output bounds, so the canvas stays eligible until the
    /// entry animation finishes.
    pub(crate) fn is_output_visually_fullscreen(&self, output: &Output) -> bool {
        if !self.is_output_fullscreen(output) {
            return false;
        }
        self.fullscreen_window_on(output).is_none_or(|window| {
            self.stage
                .id_of(&window)
                .is_none_or(|id| !self.window_animations.fullscreen_entry_active(id))
        })
    }

    pub(crate) fn tick_window_animations(&mut self, dt: Duration) {
        self.tick_window_animations_at(dt, Instant::now());
    }

    /// Advance every window animation, closing snapshot, and adoption fade.
    /// `now` is injectable so tests drive endpoint-hold deadlines deterministically.
    pub(crate) fn tick_window_animations_at(&mut self, dt: Duration, now: Instant) {
        let speed = self.config.effects.animation_speed;
        let frame_factor = 1.0 - (1.0 - speed).powf(dt.as_secs_f64() * 60.0);

        // Mark the outputs that show an animation *this* tick before advancing,
        // so the completing tick still presents the final resting frame and
        // udev re-arms the next frame (rect-scoped; never mark_all_dirty).
        let affected: Vec<Output> = self
            .space
            .outputs()
            .filter(|o| {
                let (camera, zoom) = {
                    let os = output_state(o);
                    (os.camera, os.zoom)
                };
                self.output_shows_window_animations(o, camera, zoom)
            })
            .cloned()
            .collect();

        for (id, geo) in self.window_animations.scoping_entries() {
            // An entry whose window or pin has vanished mid-chase can never be
            // ticked to convergence; drop it (same instant-complete outcome as
            // ineligible) so it can't wedge `has_active_animations` true forever.
            let resolved = self
                .stage
                .window_by_id(id)
                .cloned()
                .and_then(|element| element.client().cloned().map(|c| (element, c)));
            let Some((element, client)) = resolved else {
                self.window_animations.remove(id);
                self.drop_resize_crossfade(id);
                continue;
            };
            let live_size = client.geometry().size.to_f64();
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

    /// Resolve the outstanding request on a commit of an animated window. A
    /// commit that releases a start hold is also the crossfade's cue — the one
    /// moment the old and new pictures both exist.
    pub(crate) fn resolve_window_animation_commit(&mut self, window: &Window) {
        let Some(id) = self.stage.id_of(window) else {
            return;
        };
        let released = self
            .window_animations
            .on_window_commit(id, window.geometry().size);
        if let Some(generation) = released {
            self.start_resize_crossfade(window, id, generation);
        }
    }

    /// Clone the textures a frozen window is about to replace, so its resize leg
    /// has an old picture to fade out. Cheap (Rc clones, no GPU work) and bounded
    /// by the freeze — a commit or two; each refresh replaces the last, so the
    /// fade starts from what was actually on screen. Renderer-gated, like every
    /// capture path (the flatten needs one anyway).
    pub(crate) fn stash_resize_content(&mut self, surface: &WlSurface) {
        // This hook runs on every commit of every surface, so cheap-out on the
        // O(1) check before the surface-state read and the stage lookup.
        if !self.window_animations.is_active() {
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
    fn start_resize_crossfade(&mut self, window: &Window, id: ElementId, generation: u64) {
        let Some(capture) = self.resize_captures.take_for(id, generation) else {
            return;
        };
        let corner_clip = self.render.corner_clip_shader.clone();
        let on_screen = match self.stage.pin_of(window) {
            Some(site) => self
                .output_by_name(&site.output)
                .map_or(1.0, |o| o.current_scale().fractional_scale()),
            None => {
                let stage_pos = self.stage.position_of(window).unwrap_or_default();
                self.flatten_scale_for_canvas_rect(Rectangle::new(
                    stage_pos,
                    capture.pixels.geometry.size,
                ))
            }
        };
        // The overlay's first frame paints this bake over the frozen visual rect,
        // which can be several times the rect the content was captured at (a
        // fullscreen exit restores into a zoomed-out camera, where the rect is
        // `screen / zoom`). Rasterize for the size it will be drawn at, or the
        // fade lands visibly softer than the frozen frame it takes over from.
        let captured = capture.pixels.geometry.size;
        let stretch = self
            .window_animations
            .geometry_visual_rect(id)
            .map_or(1.0, |visual| {
                (visual.size.w / captured.w.max(1) as f64)
                    .max(visual.size.h / captured.h.max(1) as f64)
            });
        let flatten_scale = on_screen * stretch;
        let Some(mut backend) = self.backend.take() else {
            return;
        };
        let crossfade = crate::render::resize_crossfade(
            backend.renderer(),
            &capture.pixels,
            flatten_scale,
            corner_clip.as_ref(),
            capture.chrome,
        );
        self.backend = Some(backend);
        if let Some(crossfade) = crossfade {
            self.resize_crossfades.insert(id, crossfade);
        }
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

    /// Rasterization scale for a canvas rect: the max `output_scale·zoom` among
    /// outputs whose viewport intersects it (floored at 1.0).
    fn flatten_scale_for_canvas_rect(&self, rect: Rectangle<i32, Logical>) -> f64 {
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
            .fold(1.0_f64, f64::max)
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

    /// Fire held compositor action if repeat delay/rate has elapsed.
    pub fn apply_key_repeat(&mut self) {
        let Some((_, ref action, next_fire)) = self.held_action else {
            return;
        };
        let now = std::time::Instant::now();
        if now < next_fire {
            return;
        }
        let action = action.clone();
        let rate_interval = Duration::from_millis(1000 / self.config.repeat_rate.max(1) as u64);
        self.held_action.as_mut().unwrap().2 = now + rate_interval;
        self.execute_action(&action);
    }

    /// Compute focus target at the given canvas position, respecting whether
    /// the pointer is currently over a layer surface or a canvas window.
    fn focus_under(
        &self,
        canvas_pos: Point<f64, Logical>,
    ) -> Option<(FocusTarget, Point<f64, Logical>)> {
        if self.pointer_over_layer {
            let screen_pos =
                canvas::canvas_to_screen(CanvasPos(canvas_pos), self.camera(), self.zoom()).0;
            self.layer_surface_under(
                screen_pos,
                canvas_pos,
                &[
                    WlrLayer::Overlay,
                    WlrLayer::Top,
                    WlrLayer::Bottom,
                    WlrLayer::Background,
                ],
            )
        } else {
            // A resync landing on a stand-in must yield no focus, matching
            // pointer_focus_under — otherwise the hidden client gets a stray enter.
            if self.suspended_occludes(canvas_pos) {
                return None;
            }
            let window_hit = self.surface_under(canvas_pos, Some(false));
            // Pick mode: a canvas window under the pointer holds no pointer
            // focus, mirroring focus_cascade's pick guard, so every per-frame
            // resync agrees and can't hand the client its enter back. Widgets /
            // canvas layers / Bottom layers keep focus.
            if window_hit.is_some() && self.pick_mode() {
                return None;
            }
            window_hit
                .or_else(|| self.canvas_layer_under(canvas_pos))
                .or_else(|| self.surface_under(canvas_pos, Some(true)))
        }
    }

    /// Whether the focused surface holds an active pointer constraint. Motion
    /// to a locked surface reads as a phantom absolute move (snap-back).
    fn pointer_constraint_active(&self) -> bool {
        let pointer = self.seat.get_pointer().unwrap();
        pointer.current_focus().is_some_and(|focus| {
            smithay::wayland::pointer_constraints::with_pointer_constraint(
                &focus.0,
                &pointer,
                |c| c.is_some_and(|c| c.is_active()),
            )
        })
    }

    /// Keep the cursor at the same screen position after a camera or zoom
    /// change. When a constraint is active, silently update the internal
    /// location (see [`Self::pointer_constraint_active`]).
    ///
    /// A pointer grab (window move/resize, edge-pan) drives its repositioning
    /// off this motion and needs every event, so send synchronously. Otherwise
    /// the cursor is free over a sliding canvas: update the internal location
    /// now (hit-testing stays correct) but defer the client-facing motion to
    /// [`Self::flush_pointer_resync`], coalescing to one motion per frame.
    pub(crate) fn warp_pointer(&mut self, new_pos: Point<f64, Logical>) {
        let pointer = self.seat.get_pointer().unwrap();

        if self.pointer_constraint_active() {
            // A camera warp can slide another surface under a screen-fixed
            // cursor, stranding input on a stale lock. Reactivates itself once
            // the cursor returns.
            let same_surface_under_cursor = pointer.current_focus().is_some_and(|current| {
                self.focus_under(new_pos)
                    .is_some_and(|(under, _)| under == current)
            });
            if same_surface_under_cursor {
                pointer.set_location(new_pos);
                return;
            }
            if let Some(focus) = pointer.current_focus() {
                smithay::wayland::pointer_constraints::with_pointer_constraint(
                    &focus.0,
                    &pointer,
                    |c| {
                        if let Some(c) = c
                            && c.is_active()
                        {
                            c.deactivate();
                        }
                    },
                );
            }
        }

        if pointer.is_grabbed() {
            let under = self.focus_under(new_pos);
            let serial = smithay::utils::SERIAL_COUNTER.next_serial();
            pointer.motion(
                self,
                under,
                &smithay::input::pointer::MotionEvent {
                    location: new_pos,
                    serial,
                    time: self.start_time.elapsed().as_millis() as u32,
                },
            );
            pointer.frame(self);
            return;
        }

        pointer.set_location(new_pos);
        self.pending_pointer_resync = true;
    }

    /// Flush a pointer resync deferred by [`Self::warp_pointer`]. Sends a single
    /// `wl_pointer.motion` to the surface under the (already-updated) cursor,
    /// refreshing focus/hover and enter/leave. Called once per rendered frame.
    pub(crate) fn flush_pointer_resync(&mut self) {
        if !std::mem::take(&mut self.pending_pointer_resync) {
            return;
        }
        // A constraint may have activated since the deferred warp.
        if self.pointer_constraint_active() {
            return;
        }
        let pointer = self.seat.get_pointer().unwrap();
        let pos = pointer.current_location();
        let under = self.focus_under(pos);
        let serial = smithay::utils::SERIAL_COUNTER.next_serial();
        pointer.motion(
            self,
            under,
            &smithay::input::pointer::MotionEvent {
                location: pos,
                serial,
                time: self.start_time.elapsed().as_millis() as u32,
            },
        );
        pointer.frame(self);
        // Pick-mode transitions are zoom-driven, so the pick affordance won't
        // refresh on the pinch into/out of pick mode or the zoom-to-1.0
        // animation after a pick — this per-frame resync is the only pointer
        // path on every zoom writer. Gate on decoration_cursor too, not
        // pick_mode() alone: the frame that steps above the threshold must still
        // run once to clear a latched affordance, and it already reads
        // pick_mode() == false. The second disjunct is a bare bool (no hit-test)
        // and self-clears once the clear arm sets decoration_cursor = false.
        if self.pick_mode() || self.cursor.decoration_cursor {
            self.update_decoration_cursor(pos);
        }
    }

    /// Apply scroll momentum each frame. Suppressed during active
    /// PanGrab to avoid interfering with grab tracking.
    pub fn apply_scroll_momentum(&mut self, dt: Duration) {
        if self.panning() {
            return;
        }
        let delta = self.with_output_state(|os| os.momentum.tick(dt)).flatten();
        let Some(delta) = delta else {
            return;
        };

        self.set_camera(self.camera() + delta);
        self.update_output_from_camera();

        // Shift pointer canvas position so screen position stays fixed
        let pos = self.seat.get_pointer().unwrap().current_location();
        self.warp_pointer(pos + delta);
    }

    /// During a touch window-move that has reached a screen edge, re-drive the
    /// move grab from the finger's fixed screen position after the camera has
    /// edge-panned, so the window keeps following the finger. Returns true if a
    /// touch move consumed the edge-pan for `output`.
    fn redrive_touch_edge_pan(&mut self, output: &Output) -> bool {
        let Some(tep) = self.touch_state.edge_pan.clone() else {
            return false;
        };
        if &tep.output != output {
            return false;
        }
        let (camera, zoom) = {
            let os = output_state(output);
            (os.camera, os.zoom)
        };
        let location = canvas::screen_to_canvas(canvas::ScreenPos(tep.screen_pos), camera, zoom).0;
        let Some(touch) = self.seat.get_touch() else {
            return false;
        };
        let time = self.start_time.elapsed().as_millis() as u32;
        touch.motion(
            self,
            None,
            &smithay::input::touch::MotionEvent {
                slot: tep.slot,
                location,
                time,
            },
        );
        touch.frame(self);
        true
    }

    /// Apply edge auto-pan each frame during a window drag near viewport edges.
    /// Synthetic pointer motion keeps cursor at the same screen position and
    /// lets the active MoveGrab reposition the window automatically.
    pub fn apply_edge_pan(&mut self) {
        let Some(output) = self.active_output() else {
            return;
        };
        let Some(velocity) = self.effective_edge_pan_velocity(&output, Instant::now()) else {
            return;
        };
        // velocity is screen-space speed; convert to canvas delta
        let zoom = self.zoom();
        let canvas_delta = Point::from((velocity.x / zoom, velocity.y / zoom));
        self.set_camera(self.camera() + canvas_delta);
        self.update_output_from_camera();

        // Touch move: re-drive the grab instead of warping the (hidden) pointer.
        if let Some(output) = self.focused_output.clone()
            && self.redrive_touch_edge_pan(&output)
        {
            return;
        }

        let pos = self.seat.get_pointer().unwrap().current_location();
        self.warp_pointer(pos + canvas_delta);
    }

    /// Apply a viewport pan delta with momentum accumulation.
    /// Call this from any input path that should drift (scroll, click-drag, future gestures).
    /// Targets the active output (where the pointer is).
    /// `time_ms` is the libinput event timestamp (see [`canvas::VelocityTracker`]).
    pub fn drift_pan(&mut self, delta: Point<f64, Logical>, time_ms: u32) {
        self.with_output_state(|os| {
            os.camera_target = None;
            os.zoom_target = None;
            os.zoom_animation_anchor = None;
            os.overview_return = None;
            os.momentum.accumulate(delta, time_ms);
            os.camera.x += delta.x;
            os.camera.y += delta.y;
        });
        self.update_output_from_camera();
        self.schedule_momentum_timer();
    }

    /// Apply a viewport pan delta on a specific output (for grabs pinned to an output).
    /// `time_ms` is the libinput event timestamp (see [`canvas::VelocityTracker`]).
    pub fn drift_pan_on(
        &mut self,
        delta: Point<f64, Logical>,
        time_ms: u32,
        output: &smithay::output::Output,
    ) {
        {
            let mut os = super::output_state(output);
            os.camera_target = None;
            os.zoom_target = None;
            os.zoom_animation_anchor = None;
            os.overview_return = None;
            os.momentum.accumulate(delta, time_ms);
            os.camera.x += delta.x;
            os.camera.y += delta.y;
        }
        self.update_output_from_camera();
        self.schedule_momentum_timer();
    }

    /// Schedule a 50ms one-shot timer that auto-launches momentum.
    /// Covers touchpads that don't send AxisStop on finger lift.
    /// Each call resets the timer — only the last one fires.
    fn schedule_momentum_timer(&mut self) {
        if let Some(token) = self.momentum_timer.take() {
            self.loop_handle.remove(token);
        }
        let token = self
            .loop_handle
            .insert_source(
                smithay::reexports::calloop::timer::Timer::from_duration(Duration::from_millis(50)),
                |_, _, data: &mut DriftWm| {
                    data.launch_momentum();
                    smithay::reexports::calloop::timer::TimeoutAction::Drop
                },
            )
            .ok();
        self.momentum_timer = token;
    }

    fn cancel_momentum_timer(&mut self) {
        if let Some(token) = self.momentum_timer.take() {
            self.loop_handle.remove(token);
        }
    }

    /// Launch momentum on the active output — called when input ends (finger lift, gesture end).
    pub fn launch_momentum(&mut self) {
        self.cancel_momentum_timer();
        self.with_output_state(|os| os.momentum.launch());
    }

    /// Launch momentum on a specific output.
    pub fn launch_momentum_on(&mut self, output: &smithay::output::Output) {
        self.cancel_momentum_timer();
        super::output_state(output).momentum.launch();
    }

    /// Advance the camera animation toward `camera_target` using frame-rate independent lerp.
    /// Shifts the pointer by the camera delta so the cursor stays at the same screen position.
    pub fn apply_camera_animation(&mut self, dt: Duration) {
        let Some(target) = self.camera_target() else {
            return;
        };

        let old_camera = self.camera();

        let factor = self.animation_factor(dt);

        let dx = target.x - old_camera.x;
        let dy = target.y - old_camera.y;

        if dx * dx + dy * dy < 0.25 {
            self.set_camera(target);
            self.set_camera_target(None);
        } else {
            self.set_camera(Point::from((
                old_camera.x + dx * factor,
                old_camera.y + dy * factor,
            )));
        }

        self.update_output_from_camera();

        let delta = self.camera() - old_camera;
        let pos = self.seat.get_pointer().unwrap().current_location();
        self.warp_pointer(pos + delta);
    }

    /// Manage the loading cursor: activate after grace period, clear after deadline.
    pub fn check_exec_cursor_timeout(&mut self) {
        let Some(deadline) = self.cursor.exec_cursor_deadline else {
            return;
        };
        let now = Instant::now();
        if now >= deadline {
            self.cursor.exec_cursor_show_at = None;
            self.cursor.exec_cursor_deadline = None;
            self.cursor.cursor_status = CursorImageStatus::default_named();
            // The Wait cursor was what kept the loop spinning; without a dirty mark
            // the last animated frame would stay on screen until another wake.
            self.mark_all_dirty();
        } else if let Some(show_at) = self.cursor.exec_cursor_show_at
            && now >= show_at
        {
            self.cursor.exec_cursor_show_at = None;
            self.cursor.cursor_status =
                CursorImageStatus::Named(smithay::input::pointer::CursorIcon::Wait);
        }
    }

    /// Advance zoom animation toward `zoom_target` using frame-rate independent lerp.
    /// When `zoom_animation_anchor` is set (combined zoom+camera animation), keeps
    /// its screen-space anchor stable while deriving camera, preventing drift.
    /// Otherwise just adjusts pointer so cursor stays at the same screen position.
    pub fn apply_zoom_animation(&mut self, dt: Duration) {
        let Some(target) = self.zoom_target() else {
            return;
        };

        let old_zoom = self.zoom();
        let old_camera = self.camera();

        let factor = self.animation_factor(dt);

        let dz = target - old_zoom;
        let zoom_close = dz.abs() < 0.001;
        if zoom_close {
            self.set_zoom(target);
            if self.zoom_animation_anchor().is_none() {
                self.set_zoom_target(None);
            }
        } else {
            self.set_zoom(old_zoom + dz * factor);
        }

        if let Some(anchor) = self.zoom_animation_anchor() {
            // Combined zoom+camera: lerp the canvas point at the fixed screen
            // anchor, then derive camera. The anchor can be the viewport center
            // (keyboard/fit) or the pointer position (wheel zoom).
            let current_anchor: Point<f64, Logical> = Point::from((
                old_camera.x + anchor.screen.x / old_zoom,
                old_camera.y + anchor.screen.y / old_zoom,
            ));
            let cx = current_anchor.x + (anchor.canvas.x - current_anchor.x) * factor;
            let cy = current_anchor.y + (anchor.canvas.y - current_anchor.y) * factor;

            let cur_zoom = self.zoom();
            self.set_camera(Point::from((
                cx - anchor.screen.x / cur_zoom,
                cy - anchor.screen.y / cur_zoom,
            )));
            self.update_output_from_camera();

            // Suppress camera_animation — we set camera directly
            self.set_camera_target(None);

            let center_dx = anchor.canvas.x - current_anchor.x;
            let center_dy = anchor.canvas.y - current_anchor.y;
            if zoom_close && center_dx * center_dx + center_dy * center_dy < 0.25 {
                // Finish both coordinates together to avoid a camera-only tail.
                let cur_zoom = self.zoom();
                let final_camera = Point::from((
                    anchor.canvas.x - anchor.screen.x / cur_zoom,
                    anchor.canvas.y - anchor.screen.y / cur_zoom,
                ));
                self.set_zoom_target(None);
                self.clear_zoom_animation_anchor();
                self.set_camera(final_camera);
                self.update_output_from_camera();
            }

            // Warp pointer: compensate for both camera and zoom change
            let pos = self.seat.get_pointer().unwrap().current_location();
            let screen_x = (pos.x - old_camera.x) * old_zoom;
            let screen_y = (pos.y - old_camera.y) * old_zoom;
            let cur_zoom = self.zoom();
            let cur_camera = self.camera();
            let new_pos = Point::from((
                screen_x / cur_zoom + cur_camera.x,
                screen_y / cur_zoom + cur_camera.y,
            ));
            self.warp_pointer(new_pos);
        } else if self.zoom() != old_zoom {
            // Standalone zoom: just compensate pointer for zoom change
            let pos = self.seat.get_pointer().unwrap().current_location();
            let cur_camera = self.camera();
            let screen_x = (pos.x - cur_camera.x) * old_zoom;
            let screen_y = (pos.y - cur_camera.y) * old_zoom;
            let cur_zoom = self.zoom();
            let new_pos = Point::from((
                screen_x / cur_zoom + cur_camera.x,
                screen_y / cur_zoom + cur_camera.y,
            ));
            self.warp_pointer(new_pos);
        }
    }

    // -- Multi-output animation ticking (udev backend) --
    // The existing apply_* methods above operate on active_output() and are used
    // by the winit backend (single output, timer-based). Winit gets away with
    // tick-in-render because it's always single-output with a fixed timer.

    /// Tick all per-output animations once per iteration.
    /// Called from udev render_if_needed() before any render_frame() calls.
    pub fn tick_all_animations(&mut self) {
        let now = Instant::now();
        let dt = (now - self.last_animation_tick).min(Duration::from_millis(33));
        self.last_animation_tick = now;

        // Global (not per-output) ticks
        self.apply_key_repeat();
        self.check_exec_cursor_timeout();
        self.tick_window_animations(dt);
        // Re-arm cursor edge-pan from the current cursor position before the
        // per-output velocities are applied below (disarms outputs the cursor
        // has left; keeps the active output's speed stable frame-to-frame).
        self.refresh_cursor_edge_pan();

        let outputs: Vec<Output> = self.space.outputs().cloned().collect();
        let active = self.active_output();

        for output in &outputs {
            let is_active = active.as_ref().is_some_and(|a| a == output);

            {
                let mut os = output_state(output);
                os.last_frame_instant = now;
            }

            self.tick_scroll_momentum_on(output, is_active, dt);
            self.tick_edge_pan_on(output, is_active);
            // A fullscreen output's camera is locked (set_camera_on refuses to
            // move it). Drop any pending pan/zoom target so it can't fire the
            // moment fullscreen exits; the ticks then no-op on the None targets.
            if self.is_output_fullscreen(output) {
                let mut os = output_state(output);
                os.camera_target = None;
                os.zoom_target = None;
                os.zoom_animation_anchor = None;
            }
            self.tick_zoom_animation_on(output, is_active, dt);
            self.tick_camera_animation_on(output, is_active, dt);
        }

        // Single camera sync after all outputs are ticked (avoids N×M redundancy)
        self.update_output_from_camera();
    }

    fn tick_scroll_momentum_on(&mut self, output: &Output, is_active: bool, dt: Duration) {
        {
            let os = output_state(output);
            if os.panning {
                return;
            }
        }

        let delta = {
            let mut os = output_state(output);
            os.momentum.tick(dt)
        };
        let Some(delta) = delta else { return };

        let cam = output_state(output).camera;
        self.set_camera_on(output, Point::from((cam.x + delta.x, cam.y + delta.y)));

        if is_active {
            let pos = self.seat.get_pointer().unwrap().current_location();
            self.warp_pointer(pos + delta);
        }
    }

    fn tick_edge_pan_on(&mut self, output: &Output, is_active: bool) {
        let Some(velocity) = self.effective_edge_pan_velocity(output, Instant::now()) else {
            return;
        };
        let canvas_delta = {
            let os = output_state(output);
            Point::from((velocity.x / os.zoom, velocity.y / os.zoom))
        };

        let cam = output_state(output).camera;
        self.set_camera_on(
            output,
            Point::from((cam.x + canvas_delta.x, cam.y + canvas_delta.y)),
        );

        // Touch move: re-drive the grab instead of warping the (hidden) pointer.
        if self.redrive_touch_edge_pan(output) {
            return;
        }

        if is_active {
            let pos = self.seat.get_pointer().unwrap().current_location();
            self.warp_pointer(pos + canvas_delta);
        }
    }

    fn tick_camera_animation_on(&mut self, output: &Output, is_active: bool, dt: Duration) {
        let (target, old_camera) = {
            let os = output_state(output);
            let Some(target) = os.camera_target else {
                return;
            };
            (target, os.camera)
        };

        let factor = self.animation_factor(dt);

        let dx = target.x - old_camera.x;
        let dy = target.y - old_camera.y;

        let new_camera = if dx * dx + dy * dy < 0.25 {
            output_state(output).camera_target = None;
            target
        } else {
            Point::from((old_camera.x + dx * factor, old_camera.y + dy * factor))
        };
        self.set_camera_on(output, new_camera);

        if is_active {
            let new_camera = output_state(output).camera;
            let delta = new_camera - old_camera;
            let pos = self.seat.get_pointer().unwrap().current_location();
            self.warp_pointer(pos + delta);
        }
    }

    fn tick_zoom_animation_on(&mut self, output: &Output, is_active: bool, dt: Duration) {
        let (target, old_zoom, old_camera, anim_anchor) = {
            let os = output_state(output);
            let Some(target) = os.zoom_target else { return };
            (target, os.zoom, os.camera, os.zoom_animation_anchor)
        };

        let factor = self.animation_factor(dt);

        let dz = target - old_zoom;
        let zoom_close = dz.abs() < 0.001;
        {
            let mut os = output_state(output);
            if zoom_close {
                os.zoom = target;
                if anim_anchor.is_none() {
                    os.zoom_target = None;
                }
                drop(os);
            } else {
                os.zoom = old_zoom + dz * factor;
            }
        }

        if let Some(anchor) = anim_anchor {
            let current_anchor: Point<f64, Logical> = Point::from((
                old_camera.x + anchor.screen.x / old_zoom,
                old_camera.y + anchor.screen.y / old_zoom,
            ));
            let cx = current_anchor.x + (anchor.canvas.x - current_anchor.x) * factor;
            let cy = current_anchor.y + (anchor.canvas.y - current_anchor.y) * factor;

            let cur_zoom = output_state(output).zoom;
            self.set_camera_on(
                output,
                Point::from((
                    cx - anchor.screen.x / cur_zoom,
                    cy - anchor.screen.y / cur_zoom,
                )),
            );
            {
                let mut os = output_state(output);
                // Suppress camera_animation — we set camera directly
                os.camera_target = None;

                let center_dx = anchor.canvas.x - current_anchor.x;
                let center_dy = anchor.canvas.y - current_anchor.y;
                if zoom_close && center_dx * center_dx + center_dy * center_dy < 0.25 {
                    let final_camera = Point::from((
                        anchor.canvas.x - anchor.screen.x / cur_zoom,
                        anchor.canvas.y - anchor.screen.y / cur_zoom,
                    ));
                    os.zoom_target = None;
                    os.zoom_animation_anchor = None;
                    drop(os);
                    self.set_camera_on(output, final_camera);
                }
            }

            if is_active {
                let (cur_zoom, cur_camera) = {
                    let os = output_state(output);
                    (os.zoom, os.camera)
                };
                let pos = self.seat.get_pointer().unwrap().current_location();
                let screen_x = (pos.x - old_camera.x) * old_zoom;
                let screen_y = (pos.y - old_camera.y) * old_zoom;
                let new_pos = Point::from((
                    screen_x / cur_zoom + cur_camera.x,
                    screen_y / cur_zoom + cur_camera.y,
                ));
                self.warp_pointer(new_pos);
            }
        } else {
            let cur_zoom = output_state(output).zoom;
            if cur_zoom != old_zoom && is_active {
                let cur_camera = output_state(output).camera;
                let pos = self.seat.get_pointer().unwrap().current_location();
                let screen_x = (pos.x - cur_camera.x) * old_zoom;
                let screen_y = (pos.y - cur_camera.y) * old_zoom;
                let new_pos = Point::from((
                    screen_x / cur_zoom + cur_camera.x,
                    screen_y / cur_zoom + cur_camera.y,
                ));
                self.warp_pointer(new_pos);
            }
        }
    }
}
