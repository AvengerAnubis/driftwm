use std::rc::Rc;

use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::element::memory::{
    MemoryRenderBuffer, MemoryRenderBufferRenderElement,
};
use smithay::backend::renderer::element::texture::{TextureBuffer, TextureRenderElement};
use smithay::backend::renderer::element::{Kind, RenderElement};
use smithay::backend::renderer::gles::{GlesRenderer, GlesTexture};
use smithay::backend::renderer::utils::{RendererSurfaceStateUserData, import_surface};
use smithay::backend::renderer::{Bind as _, Color32F, Frame as _, Offscreen, Renderer as _};
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::user_data::UserDataMap;
use smithay::utils::{Logical, Physical, Point, Rectangle, Scale, Size, Transform};
use smithay::wayland::compositor::{TraversalAction, with_surface_tree_downward};

use crate::state::SuspendedWindow;

use super::{OutputRenderElements, WindowTransformElement};

/// A progress-based effect ends when within this of 1.0 (a 1% alpha residue is
/// invisible; a tighter epsilon leaves a long, dead tail past the motion).
const DONE_EPSILON: f64 = 0.01;

/// One surface of a captured window tree: an Rc-cloned GL texture and where it
/// sits relative to the window's render origin (logical, pre-scale).
struct BakedSurface {
    buffer: TextureBuffer<GlesTexture>,
    location: Point<f64, Logical>,
    src: Rectangle<f64, Logical>,
    dst: Size<i32, Logical>,
}

/// Content textures captured from a window's surface tree while its buffers are
/// still imported. Keyed by root surface id and consumed at teardown so the
/// close animation is independent of Wayland destruction order.
pub(crate) struct ClosePixels {
    surfaces: Vec<BakedSurface>,
    /// Logical bounds of the captured content, relative to the window origin.
    bounds: Rectangle<f64, Logical>,
}

/// Clone the already-imported textures of `surface`'s tree. A held
/// `GlesTexture` clone stays renderable for the renderer's lifetime even after
/// the surface's buffers are evicted. Returns `None` for a never-drawn tree
/// (no importable buffers).
pub(crate) fn capture_close_pixels(
    renderer: &mut GlesRenderer,
    surface: &WlSurface,
) -> Option<ClosePixels> {
    let mut surfaces: Vec<BakedSurface> = Vec::new();
    with_surface_tree_downward(
        surface,
        Point::<f64, Logical>::from((0.0, 0.0)),
        |_, states, location| {
            let mut location = *location;
            let Some(data) = states.data_map.get::<RendererSurfaceStateUserData>() else {
                return TraversalAction::SkipChildren;
            };
            // Bind the view out of the guard first; a guard held in the match
            // scrutinee would live to the arm end (re-entrant-lock hazard).
            let view = data.lock().unwrap().view();
            match view {
                Some(view) => {
                    location += view.offset.to_f64();
                    TraversalAction::DoChildren(location)
                }
                None => TraversalAction::SkipChildren,
            }
        },
        |_, states, location| {
            let mut location = *location;
            let Some(data) = states.data_map.get::<RendererSurfaceStateUserData>() else {
                return;
            };
            let Some(view) = data.lock().unwrap().view() else {
                return;
            };
            location += view.offset.to_f64();
            if import_surface(renderer, states).is_err() {
                return;
            }
            let data = data.lock().unwrap();
            let Some(texture) = data.texture(renderer.context_id()) else {
                return;
            };
            let buffer = TextureBuffer::from_texture(
                renderer,
                texture.clone(),
                data.buffer_scale(),
                data.buffer_transform(),
                None,
            );
            surfaces.push(BakedSurface {
                buffer,
                location,
                src: view.src,
                dst: view.dst,
            });
        },
        |_, _, _| true,
    );
    if surfaces.is_empty() {
        return None;
    }
    let bounds = surfaces
        .iter()
        .map(|s| Rectangle::new(s.location, s.dst.to_f64()))
        .reduce(|a, b| a.merge(b))
        .filter(|b| b.size.w > 0.0 && b.size.h > 0.0)?;
    Some(ClosePixels { surfaces, bounds })
}

/// A short-lived flattened snapshot of a closed window, animated as one texture
/// after the window has left the stage. Canvas-space so mixed-DPI outputs each
/// place it through their own camera/zoom.
pub(crate) struct ClosingSnapshot {
    buffer: TextureBuffer<GlesTexture>,
    /// Full extent in canvas coordinates. Meaningful only for a normal
    /// (non-pinned) close; a pinned/fullscreen snapshot leaves this default and
    /// scopes by `pinned` instead (its rect lives in screen space).
    canvas_rect: Rectangle<f64, Logical>,
    /// `Some((output, screen_rect))` for pinned/fullscreen closes, which render
    /// only on their home output under zoom 1.
    pinned: Option<(String, Rectangle<i32, Logical>)>,
    /// Fade in place at scale 1 (the close→stand-in conversion crossfade).
    alpha_only: bool,
    /// Shrink amplitude for a normal close (`effects.animation_scale`).
    scale_amplitude: f64,
    progress: f64,
}

impl ClosingSnapshot {
    pub fn tick(&mut self, frame_factor: f64) {
        self.progress += (1.0 - self.progress) * frame_factor;
    }

    pub fn is_done(&self) -> bool {
        1.0 - self.progress <= DONE_EPSILON
    }

    /// Canvas bounds for per-output intersection scoping. Callers must check
    /// [`Self::pinned_output`] first — this is unspecified for a pinned snapshot.
    pub fn canvas_rect(&self) -> Rectangle<f64, Logical> {
        self.canvas_rect
    }

    pub fn pinned_output(&self) -> Option<&str> {
        self.pinned.as_ref().map(|(o, _)| o.as_str())
    }
}

/// The optional SSD title bar to bake with the body: its still-alive
/// `MemoryRenderBuffer` and its rect in surface-origin-local logical coords
/// (above the content). Border/shadow stay excluded — their caches are purged
/// at teardown and keyed by surface (accepted).
type SsdBar<'a> = Option<(&'a MemoryRenderBuffer, Rectangle<f64, Logical>)>;

/// Rasterize captured content (+ optional SSD bar) into one offscreen texture.
/// Returns the texture and its extent in surface-origin-local logical coords,
/// or `None` if the offscreen can't be created.
fn flatten(
    renderer: &mut GlesRenderer,
    pixels: &ClosePixels,
    flatten_scale: f64,
    ssd_bar: SsdBar<'_>,
) -> Option<(TextureBuffer<GlesTexture>, Rectangle<f64, Logical>)> {
    let scale = Scale::from(flatten_scale);
    let bounds = match ssd_bar {
        Some((_, bar_rect)) => pixels.bounds.merge(bar_rect),
        None => pixels.bounds,
    };
    let phys_size: Size<i32, Physical> = Size::from((
        (bounds.size.w * flatten_scale).ceil() as i32,
        (bounds.size.h * flatten_scale).ceil() as i32,
    ));
    if phys_size.w <= 0 || phys_size.h <= 0 {
        return None;
    }
    let buffer_size = phys_size.to_logical(1).to_buffer(1, Transform::Normal);
    let mut texture =
        Offscreen::<GlesTexture>::create_buffer(renderer, Fourcc::Abgr8888, buffer_size).ok()?;
    // Build the bar element (needs `renderer`) before the frame borrows it.
    let bar_element = ssd_bar.and_then(|(buf, bar_rect)| {
        let loc = bar_rect.loc - bounds.loc;
        MemoryRenderBufferRenderElement::from_buffer(
            renderer,
            Point::<f64, Physical>::from((loc.x * flatten_scale, loc.y * flatten_scale)),
            buf,
            None,
            None,
            Some(bar_rect.size.to_i32_round()),
            Kind::Unspecified,
        )
        .ok()
    });
    let draw = |frame: &mut smithay::backend::renderer::gles::GlesFrame<'_, '_>,
                element: &dyn RenderElement<GlesRenderer>| {
        let src = element.src();
        let dst = element.geometry(scale);
        let Some(mut local) = Rectangle::from_size(phys_size).intersection(dst) else {
            return;
        };
        local.loc -= dst.loc;
        let cache = UserDataMap::new();
        let _ = element.draw(frame, src, dst, &[local], &[], Some(&cache));
    };
    {
        let mut target = renderer.bind(&mut texture).ok()?;
        let mut frame = renderer
            .render(&mut target, phys_size, Transform::Normal)
            .ok()?;
        let _ = frame.clear(Color32F::TRANSPARENT, &[Rectangle::from_size(phys_size)]);
        // The surface tree walks top-most first; an offscreen is painter's
        // order (bottom paints first), so draw in reverse.
        for surface in pixels.surfaces.iter().rev() {
            let loc = surface.location - bounds.loc;
            let element = TextureRenderElement::from_texture_buffer(
                Point::<f64, Physical>::from((loc.x * flatten_scale, loc.y * flatten_scale)),
                &surface.buffer,
                None,
                Some(surface.src),
                Some(surface.dst),
                Kind::Unspecified,
            );
            draw(&mut frame, &element);
        }
        // The bar sits above the content (disjoint), so it fades as one piece.
        if let Some(ref element) = bar_element {
            draw(&mut frame, element);
        }
        let _ = frame.finish();
    }
    let buffer = TextureBuffer::from_texture(renderer, texture, 1, Transform::Normal, None);
    Some((buffer, bounds))
}

/// Build a closing snapshot for a normal (canvas) window from captured pixels.
#[allow(clippy::too_many_arguments)]
pub(crate) fn snapshot_canvas(
    renderer: &mut GlesRenderer,
    pixels: &ClosePixels,
    window_origin: Point<f64, Logical>,
    flatten_scale: f64,
    scale_amplitude: f64,
    alpha_only: bool,
    ssd_bar: SsdBar<'_>,
) -> Option<ClosingSnapshot> {
    let (buffer, bounds) = flatten(renderer, pixels, flatten_scale.max(1.0), ssd_bar)?;
    let canvas_rect = Rectangle::new(window_origin + bounds.loc, bounds.size);
    Some(ClosingSnapshot {
        buffer,
        canvas_rect,
        pinned: None,
        alpha_only,
        scale_amplitude,
        progress: 0.0,
    })
}

/// Build a closing snapshot pinned to one output's screen space (pinned or
/// fullscreen closes), rendered there under zoom 1.
#[allow(clippy::too_many_arguments)]
pub(crate) fn snapshot_screen(
    renderer: &mut GlesRenderer,
    pixels: &ClosePixels,
    output: String,
    screen_origin: Point<i32, Logical>,
    flatten_scale: f64,
    scale_amplitude: f64,
    alpha_only: bool,
    ssd_bar: SsdBar<'_>,
) -> Option<ClosingSnapshot> {
    let (buffer, bounds) = flatten(renderer, pixels, flatten_scale.max(1.0), ssd_bar)?;
    let loc = Point::from((
        screen_origin.x + bounds.loc.x.round() as i32,
        screen_origin.y + bounds.loc.y.round() as i32,
    ));
    let screen_rect = Rectangle::new(loc, bounds.size.to_i32_round());
    Some(ClosingSnapshot {
        buffer,
        canvas_rect: Rectangle::default(),
        pinned: Some((output, screen_rect)),
        alpha_only,
        scale_amplitude,
        progress: 0.0,
    })
}

impl ClosingSnapshot {
    /// The render element for this snapshot on `output`, or `None` if it does
    /// not belong there.
    fn render_element(
        &self,
        output_name: &str,
        camera: Point<f64, Logical>,
        zoom: f64,
        output_scale: f64,
    ) -> Option<OutputRenderElements> {
        let alpha = (1.0 - self.progress).clamp(0.0, 1.0) as f32;
        let close_scale = if self.alpha_only {
            1.0
        } else {
            1.0 - (1.0 - self.scale_amplitude) * self.progress
        };

        let (screen_loc, screen_size): (Point<f64, Logical>, Size<f64, Logical>) =
            if let Some((pin_output, screen_rect)) = &self.pinned {
                if pin_output != output_name {
                    return None;
                }
                (screen_rect.loc.to_f64(), screen_rect.size.to_f64())
            } else {
                (
                    Point::from((
                        (self.canvas_rect.loc.x - camera.x) * zoom,
                        (self.canvas_rect.loc.y - camera.y) * zoom,
                    )),
                    Size::from((
                        self.canvas_rect.size.w * zoom,
                        self.canvas_rect.size.h * zoom,
                    )),
                )
            };

        let loc_phys: Point<f64, Physical> =
            Point::from((screen_loc.x * output_scale, screen_loc.y * output_scale));
        let size_phys: Size<f64, Physical> =
            Size::from((screen_size.w * output_scale, screen_size.h * output_scale));
        let center = Point::from((
            loc_phys.x + size_phys.w / 2.0,
            loc_phys.y + size_phys.h / 2.0,
        ));
        let texture = TextureRenderElement::from_texture_buffer(
            loc_phys,
            &self.buffer,
            Some(alpha),
            None,
            Some(screen_size.to_i32_round()),
            Kind::Unspecified,
        );
        Some(OutputRenderElements::ClosingWindow(
            WindowTransformElement::new(
                texture,
                center,
                Point::default(),
                Scale::from(close_scale),
            ),
        ))
    }
}

/// Elements for every closing snapshot visible on `output`, top-most first.
pub(crate) fn render_snapshots_for_output(
    snapshots: &[ClosingSnapshot],
    output_name: &str,
    visible: Rectangle<i32, Logical>,
    camera: Point<f64, Logical>,
    zoom: f64,
    output_scale: f64,
) -> Vec<OutputRenderElements> {
    snapshots
        .iter()
        .filter(|s| {
            s.pinned.as_ref().map_or_else(
                || visible.overlaps(s.canvas_rect.to_i32_round()),
                |(o, _)| o == output_name,
            )
        })
        .filter_map(|s| s.render_element(output_name, camera, zoom, output_scale))
        .collect()
}

/// A departing suspended stand-in fading out over the live window that adopted
/// its slot. Rendered via `push_suspended_element` with a decreasing alpha.
pub(crate) struct AdoptionFade {
    pub suspended: Rc<SuspendedWindow>,
    pub loc: Point<i32, Logical>,
    pub progress: f64,
}

impl AdoptionFade {
    pub fn tick(&mut self, frame_factor: f64) {
        self.progress += (1.0 - self.progress) * frame_factor;
    }

    pub fn is_done(&self) -> bool {
        1.0 - self.progress <= DONE_EPSILON
    }

    pub fn alpha(&self) -> f32 {
        (1.0 - self.progress).clamp(0.0, 1.0) as f32
    }
}
