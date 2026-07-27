//! Discarding the recenter a window still owes from a fullscreen, fit or fill
//! exit it has not acked.
//!
//! The exit registers a `pending_recenter` so the client's next
//! differently-sized commit lands the window on its pre-exit visual center.
//! Anything that establishes a new placement in the meantime has to discard
//! that promise, or it fires later and undoes the placement.

use smithay::desktop::Window;
use smithay::reexports::wayland_server::Resource;
use smithay::wayland::seat::WaylandFocus;

use super::DriftWm;

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
    pub(crate) fn drop_owed_recenter(&mut self, window: &Window) {
        if let Some(surface) = window.wl_surface() {
            self.pending_recenter.remove(&surface.id());
        }
    }
}
