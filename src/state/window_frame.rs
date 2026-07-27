//! Window frame geometry: SSD title-bar height, border widths, and the
//! conversions between a content top-left and the visual center of the frame
//! that strip sits above.
//!
//! [`visual_frame_center`] and [`frame_loc_for_center`] are inverses, shared by
//! navigation, fit, fill, and the fullscreen-exit settle so the formula cannot
//! drift between them.

use smithay::desktop::Window;
use smithay::reexports::wayland_server::Resource;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Logical, Point, Size};
use smithay::wayland::seat::WaylandFocus;

use driftwm::window_ext::WindowExt;

use super::{DriftWm, StageWindow};

impl DriftWm {
    pub fn window_ssd_bar<W: WaylandFocus + WindowExt>(&self, window: &W) -> i32 {
        // Every stand-in draws the same textless bar (a CSD-origin one shrinks
        // its body under it), so a suspended element always carries the bar
        // height regardless of origin.
        if window.is_suspended() {
            return self.config.decorations.title_bar_height;
        }
        window
            .wl_surface()
            .filter(|s| {
                self.decorations
                    .contains_key(&crate::decorations::DecorationKey::Surface(s.id()))
            })
            .map_or(0, |_| self.config.decorations.title_bar_height)
    }

    /// Border width for an element with no surface to resolve a per-rule
    /// override against — a suspended window. Uses the global default mode's
    /// width, matching what a relaunched client would get before its rule
    /// re-applies.
    pub fn default_border_width(&self) -> i32 {
        let mode =
            driftwm::config::effective_decoration_mode(None, &self.config.decorations.default_mode);
        driftwm::config::effective_border_width(None, mode, &self.config.decorations)
    }

    /// Border width for any stage element: the per-rule width for a client, the
    /// global default for a surfaceless stand-in.
    pub fn element_border_width(&self, w: &StageWindow) -> i32 {
        match w {
            StageWindow::Client(c) => c.wl_surface().map_or(0, |s| self.window_border_width(&s)),
            StageWindow::Suspended(_) => self.default_border_width(),
        }
    }

    /// Per-window border width, resolving rule override against
    /// `[decorations] border_width`. Returns 0 when the effective decoration
    /// mode is `None` (hard veto — per-window overrides ignored).
    pub fn window_border_width(&self, surface: &WlSurface) -> i32 {
        let applied = driftwm::config::applied_rule(surface);
        let mode = driftwm::config::effective_decoration_mode(
            applied.as_ref().and_then(|r| r.decoration.as_ref()),
            &self.config.decorations.default_mode,
        );
        driftwm::config::effective_border_width(applied.as_ref(), mode, &self.config.decorations)
    }

    /// Visual center accounting for SSD title bar above content. Sized from
    /// [`configured_window_size`], so a center taken right after a fullscreen
    /// exit describes the restored window rather than the viewport the client is
    /// still reporting.
    pub fn window_visual_center(&self, window: &Window) -> Option<Point<f64, Logical>> {
        let loc = self.stage.position_of(window)?;
        let size = configured_window_size(window);
        let bar = self.window_ssd_bar(window) as f64;
        Some(visual_frame_center(loc, size, bar))
    }
}

/// Center of the visual frame (content plus the SSD title-bar strip above it)
/// from a content top-left, content size, and bar height. Inverse of
/// [`frame_loc_for_center`]. Shared by `window_visual_center`, `nav_center`, and
/// the fit/fill/fullscreen exit settles so the formula can't drift.
pub(crate) fn visual_frame_center(
    loc: Point<i32, Logical>,
    size: Size<i32, Logical>,
    bar: f64,
) -> Point<f64, Logical> {
    Point::from((
        loc.x as f64 + size.w as f64 / 2.0,
        loc.y as f64 - bar + (size.h as f64 + bar) / 2.0,
    ))
}

/// The size a window will have once it acks everything already configured: the
/// last size we sent, else its committed geometry.
///
/// `Window::geometry()` reports the last *committed* buffer, which lags a
/// configure round-trip. That lag is invisible most of the time but not after a
/// fullscreen exit: the exit only sends the smaller configure, so a geometry
/// action dispatched right behind it (the `execute_action` guard exits first)
/// would size and center against the still-reported viewport. The server's
/// pending state is what the window is becoming, so prefer it.
pub(crate) fn configured_window_size(window: &Window) -> Size<i32, Logical> {
    window
        .toplevel()
        .and_then(|toplevel| toplevel.with_pending_state(|state| state.size))
        .filter(|size| size.w > 0 && size.h > 0)
        .unwrap_or_else(|| window.geometry().size)
}

/// Content top-left that places a frame of `size` (plus its `bar` strip) so its
/// visual center lands on `center`. Inverse of [`visual_frame_center`]; used by
/// the fit exit and the pending-recenter completion to re-place a window around
/// a preserved center.
pub(crate) fn frame_loc_for_center(
    center: Point<f64, Logical>,
    size: Size<i32, Logical>,
    bar: i32,
) -> Point<i32, Logical> {
    let total_h = size.h + bar;
    Point::from((
        (center.x - size.w as f64 / 2.0) as i32,
        (center.y - total_h as f64 / 2.0) as i32 + bar,
    ))
}
