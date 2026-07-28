//! The recenter a window owes from a fullscreen, fit or fill exit it has not
//! acked: registering it, and discarding it.
//!
//! The exit registers a `pending_recenter` so the client's next
//! differently-sized commit lands the window on its pre-exit visual center.
//! Anything that establishes a new placement in the meantime has to discard
//! that promise, or it fires later and undoes the placement.

use smithay::reexports::wayland_server::Resource;
use smithay::utils::{Logical, Point, Size};
use smithay::wayland::seat::WaylandFocus;

use super::{DriftWm, PendingRecenter, StageWindow};

impl DriftWm {
    /// Discard a recenter this window still owes from a fullscreen/fit/fill
    /// exit it has not acked.
    ///
    /// Call this from anywhere that establishes a new placement. The owed
    /// recenter fires on the client's next differently-sized commit and maps
    /// the window back to its pre-exit center — undoing the placement being
    /// established here, at an unpredictable later moment. Dropping beats
    /// merely skipping: an entry left owed also gates
    /// `reflow_grown_snapped_window` forever.
    pub(crate) fn drop_owed_recenter<W: WaylandFocus>(&mut self, window: &W) {
        if let Some(surface) = window.wl_surface() {
            self.pending_recenter.remove(&surface.id());
        }
    }

    /// Place `window` where its fullscreen, fit or fill exit restores it, and
    /// settle the recenter that exit owes. The shared tail of all three exits;
    /// what runs before it (which configure, and whether the geometry animation
    /// is seeded before or after the map) is each exit's own business.
    ///
    /// `target_center` is the visual center the settle re-derives a location
    /// from once the client resizes, and is a parameter rather than something
    /// derived from `map_loc`: `unfit_window` maps to a location already
    /// truncated out of its center by `frame_loc_for_center`, and re-deriving
    /// would shift its center by up to a pixel per axis.
    ///
    /// `refresh_snap_rect` is true for the fit and fill exits, whose entries
    /// cached a fit/fill rect that this exit invalidates, and false for the
    /// fullscreen exit: `enter_fullscreen` caches nothing, so the cached rect
    /// is still the pre-fullscreen one this exit hands back.
    pub(crate) fn establish_exit_placement(
        &mut self,
        window: &StageWindow,
        map_loc: Point<i32, Logical>,
        saved_size: Size<i32, Logical>,
        target_center: Point<f64, Logical>,
        refresh_snap_rect: bool,
    ) {
        // The size the client is still committing. Neither the exit configure
        // nor the map below touches committed geometry, so every caller reads
        // the same value whether it captured it before or after those. An
        // enter->exit inside one frame can leave the client's first pre-exit
        // frame in flight; if its size differs from `saved_size` it completes
        // the settle early against a stale footprint.
        let pre_exit_size = window.geometry().size;

        self.map_window(window.clone(), map_loc, false);

        if saved_size == pre_exit_size {
            // The exit configure re-sends the size the client already has, so no
            // commit with a changed size will arrive to trigger the recenter —
            // the position mapped above is already final. A preceding exit can
            // have owed one already, so drop rather than merely skip.
            self.drop_owed_recenter(window);
            if refresh_snap_rect {
                // Refresh the cache the recenter completion would otherwise
                // have refreshed.
                self.refresh_stable_snap_rect(window);
            }
        } else if let Some(surface) = window.wl_surface() {
            // The client keeps committing pre-exit-sized frames until it acks
            // the restore configure; those stale-sized commits also read as
            // "grown past settled" in the reflow, which this entry gates.
            self.pending_recenter.insert(
                surface.id(),
                PendingRecenter {
                    target_center,
                    pre_exit_size,
                },
            );
        }
    }
}
